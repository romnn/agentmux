# Turns `codex app-server generate-json-schema`'s bundle into the OpenAPI 3.1 document progeny
# generates `src/appserver/schema.rs` from.
#
# Input: `codex_app_server_protocol.schemas.json`, a draft-07 bundle whose `definitions` hold the
# top-level types and, under `v2`, the v2 protocol's.
# `$roots` names the types agentmux sends or reads; only they and what they reference are kept,
# so the generated module carries 113 types rather than 721.
# `$version` is the codex CLI version the schema came from.

# The schema names referenced anywhere under the input.
def refs:
  [.. | objects | select((.["$ref"] | type) == "string") | .["$ref"]
   | sub("^#/components/schemas/"; "")];

# Every schema reachable from `.` (an array of names), by following references to a fixed point.
def closure($schemas):
  def step: . as $have | ($have + ([$have[] | $schemas[.] | refs] | add // [])) | unique;
  def fix: . as $have | ($have | step) as $next | if $next == $have then $have else ($next | fix) end;
  fix;

# Give each titled branch of a component's `oneOf` a component of its own, named by its title,
# so the generated type is named for what it is (`AgentMessageThreadItem`) rather than for its
# position in the union (`ThreadItem2`), which would change meaning if codex reordered them.
# A title already taken by a different schema is left inline.
def hoist_titled_branches:
  reduce (to_entries[] | select((.value.oneOf | type) == "array")) as $union (.;
    reduce range(0; $union.value.oneOf | length) as $index (.;
      $union.value.oneOf[$index] as $branch
      | if ($branch | type) == "object"
           and ($branch.title | type) == "string"
           and $branch.title != $union.key
           and (.[$branch.title] == null or .[$branch.title] == $branch)
        then .[$branch.title] = $branch
             | .[$union.key].oneOf[$index] = {"$ref": ("#/components/schemas/" + $branch.title)}
        else . end));

((.definitions | del(.v2)) + .definitions.v2
 | with_entries(.value |= del(.["$schema"]))
 | walk(if type == "object" and (.["$ref"] | type) == "string"
        then .["$ref"] |= sub("^#/definitions/(v2/)?"; "#/components/schemas/")
        else . end)
 | hoist_titled_branches) as $all
| ($roots | closure($all)) as $keep
| {openapi: "3.1.0",
   info: {title: "codex app-server protocol", version: $version},
   paths: {},
   components: {schemas: ($all | with_entries(select(.key as $name | $keep | index($name))))}}
