//! Which type answers each request: [`Responses`], read from codex's own request definitions.
//!
//! codex's schema bundle pairs every method with its parameters but no request with its result,
//! so the pairing comes from where codex itself declares it: the `client_request_definitions!`
//! and `server_request_definitions!` tables in
//! `codex-rs/app-server-protocol/src/protocol/common.rs`.
//! [`upstream`](crate::upstream) fetches that file at the pinned release and [`Responses::parse`]
//! reads the tables, which are then vendored beside the bundle; generation reads the vendored
//! file and checks it against the bundle's request unions, method for method.

use std::collections::BTreeMap;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use serde::{Deserialize, Serialize};

use crate::Error;

/// The macro that declares the requests a client sends.
const CLIENT_REQUESTS: &str = "client_request_definitions";
/// The macro that declares the requests a server sends.
const SERVER_REQUESTS: &str = "server_request_definitions";

/// The schema of each request's `result`, by method, as codex declares it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Responses {
    /// Requests the client sends.
    pub client: BTreeMap<String, String>,
    /// Requests the server sends.
    pub server: BTreeMap<String, String>,
}

impl Responses {
    /// Read both request tables of codex's `common.rs`.
    ///
    /// The tables include experimental methods the stable bundle leaves out; generation ignores
    /// what the bundle does not list.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Definitions`] when the source is not Rust tokens, a table is missing or
    /// appears twice, an entry does not have the shape codex's macros accept, or a method is
    /// declared twice.
    pub fn parse(source: &str) -> Result<Self, Error> {
        let tokens: TokenStream = source
            .parse()
            .map_err(|err| Error::Definitions(format!("the source is not Rust: {err}")))?;
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        Ok(Self {
            client: table(&invocation(&tokens, CLIENT_REQUESTS)?)?,
            server: table(&invocation(&tokens, SERVER_REQUESTS)?)?,
        })
    }

    /// The vendored file's contents.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Definitions`] when it cannot be encoded, which a map of strings never is.
    pub fn to_json(&self) -> Result<String, Error> {
        serde_json::to_string_pretty(self)
            .map(|json| json + "\n")
            .map_err(|err| Error::Definitions(err.to_string()))
    }

    /// Read the vendored file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Definitions`] when it is not what [`Responses::to_json`] writes.
    pub fn from_json(json: &[u8]) -> Result<Self, Error> {
        serde_json::from_slice(json)
            .map_err(|err| Error::Definitions(format!("the vendored responses: {err}")))
    }

    /// Only the requests `keep` says the bundle lists, so the vendored file holds what generation
    /// reads and nothing an experimental method adds.
    #[must_use]
    pub fn retain(mut self, mut keep: impl FnMut(Side, &str) -> bool) -> Self {
        self.client.retain(|method, _| keep(Side::Client, method));
        self.server.retain(|method, _| keep(Side::Server, method));
        self
    }

    /// The table of requests `side` sends.
    #[must_use]
    pub fn sent_by(&self, side: Side) -> &BTreeMap<String, String> {
        match side {
            Side::Client => &self.client,
            Side::Server => &self.server,
        }
    }
}

/// Who sends a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The client.
    Client,
    /// The server.
    Server,
}

/// The body of the one top-level `name! { … }` in `tokens`.
fn invocation(tokens: &[TokenTree], name: &str) -> Result<Vec<TokenTree>, Error> {
    let mut found = tokens.windows(3).filter_map(|window| match window {
        [
            TokenTree::Ident(ident),
            TokenTree::Punct(bang),
            TokenTree::Group(body),
        ] if *ident == name && bang.as_char() == '!' && body.delimiter() == Delimiter::Brace => {
            Some(body.stream().into_iter().collect::<Vec<_>>())
        }
        _ => None,
    });
    match (found.next(), found.next()) {
        (Some(body), None) => Ok(body),
        (None, _) => Err(Error::Definitions(format!("there is no `{name}!` table"))),
        (Some(_), Some(_)) => Err(Error::Definitions(format!(
            "there is more than one `{name}!` table"
        ))),
    }
}

/// Every entry of one table, as method and response schema.
///
/// An entry is `#[…]* Variant (=> "method")? { key: value, … }`, separated by commas.
/// An entry without a wire name is sent under its variant's name in lower camel case, as the
/// table's `#[serde(rename_all = "camelCase")]` spells it.
fn table(body: &[TokenTree]) -> Result<BTreeMap<String, String>, Error> {
    let mut entries = BTreeMap::new();
    let mut rest = body;
    loop {
        rest = skip_attributes(rest);
        let Some((first, after)) = rest.split_first() else {
            return Ok(entries);
        };
        let TokenTree::Ident(variant) = first else {
            return Err(Error::Definitions(format!(
                "expected an entry's name, found `{first}`"
            )));
        };
        let variant = variant.to_string();
        let (method, after) = match after {
            [
                TokenTree::Punct(eq),
                TokenTree::Punct(gt),
                TokenTree::Literal(wire),
                after @ ..,
            ] if eq.as_char() == '=' && gt.as_char() == '>' => {
                (string_literal(&wire.to_string(), &variant)?, after)
            }
            after => (lower_camel(&variant), after),
        };
        let [TokenTree::Group(fields), after @ ..] = after else {
            return Err(Error::Definitions(format!(
                "`{variant}` has no `{{ … }}` body"
            )));
        };
        let response = response(&fields.stream().into_iter().collect::<Vec<_>>(), &variant)?;
        if entries.insert(method.clone(), response).is_some() {
            return Err(Error::Definitions(format!("`{method}` is declared twice")));
        }
        rest = match after {
            [TokenTree::Punct(comma), after @ ..] if comma.as_char() == ',' => after,
            after => after,
        };
    }
}

/// The schema named by an entry's `response:` field: the last segment of its type path.
fn response(fields: &[TokenTree], variant: &str) -> Result<String, Error> {
    let start = fields
        .windows(2)
        .position(|window| {
            matches!(window, [TokenTree::Ident(key), TokenTree::Punct(colon)]
                if *key == "response" && colon.as_char() == ':')
        })
        .ok_or_else(|| Error::Definitions(format!("`{variant}` declares no response")))?;
    let value = fields.get(start + 2..).unwrap_or_default();
    let end = value
        .iter()
        .position(|token| matches!(token, TokenTree::Punct(comma) if comma.as_char() == ','))
        .unwrap_or(value.len());
    let path = value.get(..end).unwrap_or_default();
    // A plain path, `v2::ConfigWriteResponse`: identifiers joined by `::`.
    let plain = path.iter().enumerate().all(|(index, token)| match token {
        TokenTree::Ident(_) => index % 3 == 0,
        TokenTree::Punct(colon) => colon.as_char() == ':' && index % 3 != 0,
        _ => false,
    });
    match path.last() {
        Some(TokenTree::Ident(name)) if plain => Ok(name.to_string()),
        _ => Err(Error::Definitions(format!(
            "`{variant}`'s response is not a type path: `{}`",
            path.iter().map(ToString::to_string).collect::<String>()
        ))),
    }
}

/// `tokens` after any leading `#[…]` attributes, doc comments included.
fn skip_attributes(mut tokens: &[TokenTree]) -> &[TokenTree] {
    while let [TokenTree::Punct(hash), TokenTree::Group(group), rest @ ..] = tokens
        && hash.as_char() == '#'
        && group.delimiter() == Delimiter::Bracket
    {
        tokens = rest;
    }
    tokens
}

/// The text of a plain string literal.
fn string_literal(literal: &str, variant: &str) -> Result<String, Error> {
    literal
        .strip_prefix('"')
        .and_then(|literal| literal.strip_suffix('"'))
        .filter(|text| !text.contains(['\\', '"']))
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::Definitions(format!(
                "`{variant}`'s method is not a plain string: {literal}"
            ))
        })
}

/// `ApplyPatchApproval` as `applyPatchApproval`.
fn lower_camel(variant: &str) -> String {
    let mut chars = variant.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_ascii_lowercase().to_string() + chars.as_str()
    })
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use indoc::indoc;

    use super::Responses;

    /// Both tables are read, in every entry shape codex's macros accept: attributes and doc
    /// comments, a wire name or none, attributed parameters, and fields around the response.
    #[gtest]
    fn both_tables_are_read_in_every_entry_shape() -> Result<()> {
        let source = indoc! {r#"
            macro_rules! client_request_definitions { ($($t:tt)*) => {}; }

            client_request_definitions! {
                Initialize => "initialize" {
                    params: v1::InitializeParams,
                    serialization: None,
                    response: v1::InitializeResponse,
                },
                #[experimental("memory/reset")]
                /// Reset memory.
                MemoryReset => "memory/reset" {
                    params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
                    serialization: global("memory"),
                    response: v2::MemoryResetResponse,
                },
                ConfigValueWrite => "config/value/write" {
                    params: v2::ConfigValueWriteParams,
                    serialization: global("config"),
                    manual_payload_conversion: manual,
                    response: v2::ConfigWriteResponse,
                },
            }

            server_request_definitions! {
                /// Sent when approval is requested.
                CommandExecutionRequestApproval => "item/commandExecution/requestApproval" {
                    params: v2::CommandExecutionRequestApprovalParams,
                    response: v2::CommandExecutionRequestApprovalResponse,
                },
                ApplyPatchApproval {
                    params: v1::ApplyPatchApprovalParams,
                    response: v1::ApplyPatchApprovalResponse,
                },
            }
        "#};
        let responses = Responses::parse(source).or_fail()?;

        // The client table, by wire name, with the response's last path segment.
        assert_that!(
            responses.client,
            unordered_elements_are![
                (eq("initialize"), eq("InitializeResponse")),
                (eq("memory/reset"), eq("MemoryResetResponse")),
                (eq("config/value/write"), eq("ConfigWriteResponse")),
            ]
        );
        // A server entry without a wire name is sent under its camel-cased variant.
        assert_that!(
            responses.server,
            unordered_elements_are![
                (
                    eq("item/commandExecution/requestApproval"),
                    eq("CommandExecutionRequestApprovalResponse")
                ),
                (eq("applyPatchApproval"), eq("ApplyPatchApprovalResponse")),
            ]
        );
        Ok(())
    }

    /// A table that is missing, a method declared twice, and a response that is not a type path
    /// each stop the read rather than vendor a guess.
    #[gtest]
    fn a_table_codex_reshaped_is_refused() {
        let cases = [
            (
                "client_request_definitions! {}",
                "no `server_request_definitions!` table",
            ),
            (
                indoc! {r#"
                    client_request_definitions! {
                        A => "a" { params: P, response: R },
                        B => "a" { params: P, response: R },
                    }
                    server_request_definitions! {}
                "#},
                "declared twice",
            ),
            (
                indoc! {r#"
                    client_request_definitions! { A => "a" { params: P, response: Option<R> } }
                    server_request_definitions! {}
                "#},
                "not a type path",
            ),
        ];
        for (source, reason) in cases {
            assert_that!(
                Responses::parse(source),
                err(displays_as(contains_substring(reason)))
            );
        }
    }
}
