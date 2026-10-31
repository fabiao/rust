# asksource

`asksource` is ASK's host tool for extracting typed Rust syntax and collecting
compiler facts from the builds that actually produce ASK artifacts. Its installed
executable is **`cargo-asksource`**. Cargo discovers it as the external subcommand
`cargo asksource`.

The tool connects source inspection to configured compiler evidence. A source
parser can describe an array declaration, but resolving its imported bound,
generic substitutions, constructor, or macro expansion may require the compiler.
`asksource` supplies both kinds of facts while preserving their different scopes
and provenance.

The package is ASK-owned tooling inside ASK's Rust fork. It is not intended for
submission to Rust upstream. The current integration contract is in
[Rust toolchain](../../../../../../../docs/rust-toolchain.md). The
[RC-00 source-model contract](../../../../../../../docs/implementations/core/static-resource-capacity.md#source-model-cutover-contract)
owns the unfinished inventory and admission replacement; this README describes
the implemented tool rather than changing that contract. The
[preview assurance policy](../../../../../../../docs/testing.md#pre-alpha-capacity-assurance)
limits which capacity evidence must block first publication. This tool still
produces complete facts for a requested capture; its correctness and freshness
checks are not relaxed. Shared admission migration remains pending.

## Purpose and boundaries

ASK uses source analysis to maintain its runtime-resource inventory and detect
unreviewed capacity restrictions. Relevant constructs include array extents,
const-generic arguments, inferred local storage, limits on measured collections,
and storage introduced through constructors or expansion. Correct analysis needs
to distinguish live occupancy limits from fixed representations, transfer quanta,
and probes that intentionally inspect only enough values to answer a question.

`asksource` provides evidence for that analysis. It does not decide whether a
number is a justified hardware constraint, an arbitrary occupancy ceiling, or a
resource defect. Those classifications belong to ASK's inventory and owning
contracts. It also does not implement a second Rust type checker, const evaluator,
package manager, or build scheduler.

The Python integration in
[`tools/rust_source_model.py`](../../../../../../../tools/rust_source_model.py)
owns validated source snapshots and projection of configured compiler facts onto
raw source. Askchef owns recipe selection and build preparation. Rust bootstrap
owns SDK build preparation and helper installation. Native Cargo owns dependency
resolution, unit scheduling, freshness, locks, and compilation.

## Implemented capabilities

### Typed source extraction

The `source` command parses supplied text with upstream `syn` and emits structured
syntax facts. It supports complete Rust files, individual expressions, and
attribute metadata. It records:

- Declarations, modules, imports, aliases, associated items, and their owners.
- Types, generic arguments, parameters, array types, and repeat extents.
- Expressions, operators, calls, receivers, field access, and constructors.
- Patterns, local declarations, initializers, blocks, and returned block tails.
- Attributes, compiler-owned builtin classification, and conditional alternatives.
- Token ranges, literal markers, source hashes, and parent relationships.

Spans refer to UTF-8 byte offsets in the supplied text. File parsing preserves
the coordinate space of byte-order marks and shebang prefixes. The Python reader
converts byte spans to character spans when needed and preserves source bytes and
line endings when loading files.
Attribute classification uses the linked compiler's builtin registry. The reader
unwraps Rust's `unsafe(...)` attribute syntax and retains unresolved expansion
requirements for custom attributes and derives. Those requirements name the
declaration owner, complete character/byte spans, and inherited cfg conditions.
Each configured view starts from the original source; filtering one configuration
cannot erase an alternative needed by another.

Parsing establishes syntax, not name resolution or successful compilation. For
example, an identifier pattern alone cannot prove that it introduces a binding
rather than naming a constant or constructor. Opaque macro tokens cannot establish
the contents of their expansion.

### Configured compiler facts

The `capture` command runs the selected compiler through `rustc_driver` with
recording callbacks. It collects facts in that compiler's actual configuration,
including:

- Resolved definitions, local bindings, selected methods, and field definitions.
- Types, generic arguments, instantiated fields, and owned inline-storage shapes.
- Initializer and pattern origins, binding modes, and constructor assignments.
- Operators, operand origins, casts, field receivers, and exact integer magnitudes.
- Source and macro call-site locations with expansion provenance.
- Concrete MIR storage, codegen-instance and inlined-owner information, including
  closure captures and coroutine storage where collected by the compiler owner.
- Compiler cfg, source-file identities, generated source, dependency artifacts,
  tracked file and environment inputs, and output artifact fingerprints.

Storage analysis stops at pointers and references. Generic argument identity does
not recursively turn every referenced type into owned inline storage. Optimized
MIR supplements source and HIR evidence; it cannot prove that a source construct
is absent merely because optimization removed it.

The helper uses private compiler APIs and must match the selected compiler. Direct
capture checks that the requested compiler is the helper's sibling `rustc`.
Separate matching helpers support the ASK compiler, the kernel's selected nightly,
and bootstrap's compiler when that compiler participates in the selected graph.

Compiler graph links use owner/local-node handles scoped to one exact validated
snapshot. They distinguish desugared nodes with identical source provenance.
Handles have no ordering meaning and cannot identify resources, catalog entries,
or nodes across builds. The reader binds references to their snapshot and checks
uniqueness, graph closure, owner agreement, and full source/expansion provenance.
Origin-only links are rejected; they cannot substitute for a missing handle.
An imported source projection can carry additional defining-unit provenance;
its handles still belong to the compiler unit that produced the graph. Graph
links resolve across files in that unit's validated stream, including
links from an imported definition to its caller. A compact index retains native
handles, byte offsets and record hashes; lookup reads one record and checks its
identity. Source-relative spans accompany only records belonging to that source
projection. No whole-crate record tree is retained for graph lookup.

### Native Cargo graph capture

`cargo asksource build` executes Cargo's normal build operation through a bounded
per-unit executor hook. It retains the selected package, target, feature, profile,
dependency, and compiler contexts, rather than reconstructing a build from source
paths or a guessed default configuration.

The executor requests compiler facts for units containing selected sources. It
can repair missing or stale facts even when Cargo considers the ordinary unit
fresh, and reuse complete matching facts. Build dependencies and target
dependencies retain their separate contexts.

An optional absolute `unit_store` in the request moves library and build-script
unit facts out of the request directory when the request has no owned sources
and no compiler shims. Those facts bind to the store's fixed request and to the
semantic unit key, so another build sharing the Cargo target directory reuses a
fresh unit instead of repairing its facts by recompiling it.

The key includes the unit's LTO mode. Cargo derives that mode from the roots
that reach a dependency, not from the dependency itself, so two recipes with
different `lto` settings build the same library as two Cargo units with equal
descriptors. Without the mode, facts recorded for one unit would vouch for the
other. Cargo computes the mode only inside `compile_with_exec`, so the
executor reproduces that computation while planning. `init` then rebuilds the
graph snapshot from the `BuildRunner`'s actual assignment, so any difference
refuses the capture as a changed graph.

Cargo announces a unit to the executor only when it compiles it. A binary that
stays fresh is therefore identified by the single graph unit whose descriptor
matches the one Cargo reports for it; no match or several matches refuse.

The selected Cargo executable must report a source commit supported by an exact
embedded adapter. The package currently contains adapters for its in-tree Cargo
and pinned bootstrap Cargo, sharing one executor implementation. An unsupported
producer fails with an error; there is no permissive adapter selection by a
similar version string.

Capture controls travel through arguments. Ordinary compiler and build-script
environments are preserved. The integration does not edit Cargo's private
fingerprint files, touch Rust source files to force work, or clean build trees to
obtain evidence.

### Bootstrap and recipe integration

SDK capture records bootstrap's prepared Cargo commands and their explicit build
modes. These include frontend, std, compiler, codegen, and tool contexts. The
Python frontend captures its build before launching the resulting executable;
Rust-side capture runs once ordinary provisioning has supplied matching helpers.
This avoids introducing another sysroot-initialization pass.

Bootstrap compiler shims have explicit path/hash authorization. A supported shim
applies its ordinary compiler selection, arguments, and environment, then invokes
the matching helper. Capture records both the original launcher context and the
effective compiler context. Unsupported nested wrappers and unknown shim
identities fail rather than being mistaken for the compiler.

Askchef similarly captures its actual prepared recipe Cargo commands, including
recursive selections and global Cargo configuration. Group completion receipts
bind the current invocation, request, producer, and selected graph collection.
Missing requested roots, failed groups, and stale receipts prevent completion.

## Commands and input

| Command | Role |
| --- | --- |
| `cargo asksource source` | Read a JSON request from stdin and write typed syntax facts to stdout. |
| `cargo asksource identity` | Print the discovered executable's path and SHA-256 as JSON. |
| `cargo asksource build --capture-request <request> [Cargo options]` | Build with the actual selected Cargo producer and collect configured facts. |
| `cargo-asksource capture --output <absolute-path> [--request <request> --request-sha256 <hash>] -- <rustc> <arguments>` | Capture one explicit compiler invocation. |
| `cargo-asksource sdk-frontend <request> <invocation> <host> -- <prepared Cargo command>` | Internal bootstrap frontend handoff. |

Use `cargo +ask asksource ...` or `cargo +nightly asksource ...` when selecting a
Rustup toolchain. The `build` command requires invocation through Cargo so that
Cargo supplies its actual executable. Other commands can also be called directly
through the installed `cargo-asksource` path.

Cargo must be able to discover the selected helper. In particular, Rustup's Cargo
fallback for a linked toolchain does not necessarily put that toolchain's `bin`
directory on the command search path. The Python integration verifies discovery
with `identity` and, when necessary, adds the selected directory for discovery
only, removing that addition before Cargo executes builds. A directly addressed
helper avoids command discovery for `source` and `capture`.

The executable also accepts a sole `@<path>` argument. That file contains one
argument per line, excluding the executable name. This is internal argument
transport, not shell syntax or a Rust compiler response-file parser.

### Syntax request example

With the matching ASK helper installed:

```sh
ask_source_helper="$(rustc +ask --print sysroot)/bin/cargo-asksource"
"$ask_source_helper" source <<'JSON'
{
  "sources": [
    {
      "path": "example.rs",
      "context": "file",
      "text": "const WIDTH: usize = 4; pub struct Packet { bytes: [u8; WIDTH] }"
    }
  ]
}
JSON
```

`path` labels the supplied text; this command does not load that path from disk.
`context` defaults to `file`; the other supported values are `expression` and
`meta`. The response contains `complete: true` and a `sources` array whose records
include the path, SHA-256, syntax nodes, and token spans. A parse error fails the
request rather than returning an empty successful inventory.

### Build request shape

The build integration supplies a JSON request with these fields:

```json
{
  "directory": "/absolute/path/to/ask/temp/source-capture",
  "sources": ["/absolute/path/to/package/src/lib.rs"],
  "graph": {"purpose": "resource-analysis"},
  "compiler_shims": {}
}
```

The paths above are illustrative and must name the intended local output
directory and source files. `graph` is caller-supplied context carried into the
capture; the native executor separately records the actual selected Cargo graph.
Direct compiler builds use an empty `compiler_shims` map. Bootstrap derives
authorized shim entries from its own command owner.

Pass the request to `cargo asksource build` with the same manifest, package,
feature, target, profile, and configuration options as the intended build. For
ASK targets, preserve the required release profile without embedded debug
information. Bootstrap host prerequisites retain their normal owning profile;
capture does not override it. Prefer the existing Python, askchef, or bootstrap
integration when capturing those owners' builds.

## Evidence storage and validation

Compiler output consists of metadata plus two JSON Lines streams:

- `nodes`: compiler syntax, expression, binding, and storage facts.
- `definitions`: compiler-owned definition and declaration facts.

Each metadata descriptor names the exact stream path, SHA-256, and record count.
The writer serializes records as they are collected instead of retaining a
whole-crate JSON tree. It serializes concurrent capture to the same metadata
destination with a file lock, publishes the completed streams, and publishes
metadata after successful compilation and input/output validation. Consumers must
validate the metadata and streams together; a stream or partial file alone is not
completion evidence. The retired embedded-array representation is rejected.

Native Cargo capture publishes `build.json`; SDK and recipe owners publish
`builds.json` and `recipes.json` collections respectively. Group receipts bind
the seed request and fresh invocation token so a dry run or an older successful
build cannot stand in for the current capture.

The Python reader validates compiler/helper identity, selected graph and source
coverage, artifact and input hashes, tracked environment hashes, source-file
identity, and stream integrity before exposing configured facts. Source projection
retains the defining unit's cfg and source identity across dependency edges,
including imported or remapped source. Ambiguous or missing required facts fail
with an error. File membership, producer identity, path resolution, and tracked
inputs are rechecked through private snapshot publication.

Environment values are represented by hashes, including absence where tracked;
their plaintext values are not persisted. Facts do contain source paths, compiler
arguments, and generated-source text where applicable. Disposable captures belong
under ASK's gitignored `temp/`; retained verification belongs in the existing
evidence owners.

Artifact freshness uses the exact bytes of actual outputs. The separate exit
comparison for incremental archives permits only the accepted compiler-generated
invocation-name bijection and corresponding link references. It does not rewrite
artifacts, relax freshness hashes, or permit other payload differences.

## Building and provisioning

Build through Rust bootstrap, which supplies the private compiler libraries and
installs the helper into its matching sysroot. From the ASK checkout root:

```sh
cd recipes/tools/rust/source
BOOTSTRAP_SKIP_TARGET_SANITY=1 python3 x.py build --stage 1 \
  --target x86_64-unknown-linux-gnu,x86_64-unknown-ask \
  --set build.optimized-compiler-builtins=false \
  --set rust.deny-warnings=false library src/tools/asksource
```

This package has an isolated Cargo workspace because it embeds Cargo libraries.
Initialize the fork's pinned Cargo submodule as required by host setup. A plain
standalone `cargo build` is not the supported way to provision this
`rustc_private` tool.

For external snapshot compilers, bootstrap supports stage0 provisioning with the
`snapshot-driver` feature and a separate build directory. The provided compiler
must have its matching compiler-dev exports. Follow the exact Linux or Windows
commands in [BUILDME](../../../../../../../BUILDME.md) for nightly and bootstrap
helper provisioning; do not substitute an ASK helper for another compiler.
Reprovision after changing the selected compiler or the helper implementation.

## Source layout

| File | Responsibility |
| --- | --- |
| [`src/main.rs`](src/main.rs) | Command dispatch, argument-file transport, and exit handling. |
| [`src/syntax.rs`](src/syntax.rs) | Typed `syn` traversal, syntax edges, attributes, tokens, and spans. |
| [`src/semantic.rs`](src/semantic.rs) | Compiler callbacks, resolved/type/storage facts, input tracking, and direct capture. |
| [`src/records.rs`](src/records.rs) | Streaming compiler-record storage and hash/count descriptors. |
| [`src/cargo_build.rs`](src/cargo_build.rs) | Exact Cargo adapter selection and executable identity. |
| [`src/cargo_executor.rs`](src/cargo_executor.rs) | Shared native Cargo graph capture, fact reuse/repair, and build completion. |
| [`src/launcher.rs`](src/launcher.rs) | Shared request and compiler-shim context handling. |
| [`src/sdk.rs`](src/sdk.rs) | Shared SDK capture executor and frontend receipts. |
| [`src/bootstrap.rs`](src/bootstrap.rs) | Rust bootstrap's prepared-command collection and grouped SDK completion. |
| [`src/bootstrap_config.rs`](src/bootstrap_config.rs) | Shared bootstrap capture configuration and build-mode names. |

Some integration modules are included by bootstrap and its compiler shim as well
as by this executable. Their shared implementation keeps command ownership and
receipt validation consistent across those callers.

## Verification and current limits

The retained host contract suite is
[`tools/test_rust_source_model.py`](../../../../../../../tools/test_rust_source_model.py).
From the ASK checkout root, after provisioning matching helpers:

```sh
PYTHONPATH=tools python3 -m unittest test_rust_source_model
```

The suite covers source ownership and syntax, configured compiler projection,
macro and generic storage, artifact comparison, Cargo freshness and fact repair,
shim handling, and malformed/stale/partial evidence. `ASK_SOURCE_BETA_CONTRACT=1`
adds the configured bootstrap compiler to relevant compiler cases.
`ASK_SOURCE_SDK_CONTRACT=1` enables the opt-in SDK integration case, which performs
real bootstrap builds and capture. These are build operations; preserve existing
build trees and do not impose a deadline that terminates Cargo work.

The source and compiler primitives and their build integrations are implemented.
The complete MEM-00 inventory owner/caller migration and final artifact-admission
cutover remain pending. Producing capture evidence does not itself admit an
artifact or close RC-00.

A configured build proves only its named graph and inputs. Unselected source
alternatives cannot inherit semantic proof from another configuration. Unknown
or opaque constructs remain explicit obligations. Source origin is not always
unique: compiler-generated expressions can share one owner and full origin, so
exact graph links use snapshot-local handles. Unsupported storage shapes
also remain unresolved rather than being interpreted as zero storage.

For current scope and evidence, consult the
[source-model investigation](../../../../../../../docs/reports/static-capacity-coverage-reaudit.md#2026-10-15-source-model-contract-investigation),
[testing contract](../../../../../../../docs/testing.md), and
[project roadmap](../../../../../../../docs/implementations/ROADMAP.md).
