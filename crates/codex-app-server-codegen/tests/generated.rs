//! The checked-in protocol sources are exactly what the vendored inputs generate.
//!
//! Runs without codex and without the network, so a change to the generator, the vendored bundle
//! or responses, or the pinned progeny that is not followed by `task codegen:app-server` fails
//! here rather than shipping sources nothing can reproduce.
//! Whether the vendored inputs are what the pinned codex exports is a separate question, which
//! only `codex-app-server-codegen refresh --check` can answer, because it needs that codex and its
//! source.

use codex_app_server_codegen::{GeneratorPaths, Inputs, generate};
use googletest::prelude::*;

/// Every generated file in `codex-app-server-protocol` matches what generation produces now.
#[gtest]
fn the_checked_in_protocol_is_what_the_vendored_schema_generates() -> Result<()> {
    let layout = GeneratorPaths::of_this_checkout();
    let files = generate(&Inputs::read(&layout).or_fail()?, &layout).or_fail()?;

    // Compared whole rather than with a diff: the schema module is twenty thousand lines, and the
    // regeneration command is what shows the change.
    for file in files {
        expect_that!(
            file.is_current().or_fail()?,
            eq(true),
            "{} differs from what generation produces; run `task codegen:app-server`",
            file.path.display()
        );
    }
    Ok(())
}
