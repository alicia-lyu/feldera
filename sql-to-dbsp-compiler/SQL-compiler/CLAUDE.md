# SQL compiler (fork additions)

The compiler is upstream Feldera code (Java, Calcite). This file describes only what the fork adds for merged indexes and for several circuits in one runtime. The spec language is in [general-definition.md](../../docs/design/merged-index/general-definition.md); the grammar is not restated here.

## Merged-index additions

| Piece | Location |
| --- | --- |
| `.mi` spec parser (`MergedIndexSpecParser`, `MergedIndexSpecException`) and schema validator `MergedIndexValidator` | package `org.dbsp.sqlCompiler.compiler.frontend.mergedIndex` |
| Rust emitter for a spec | `MergedIndexRustEmitter` (package `org.dbsp.sqlCompiler.compiler.backend.rust`) |
| Entry point | `CompilerMain.emitMergedIndex`, selected by `--merged-index <spec>` and `--merged-index-output <file>` |

- `MergedIndexValidator` is the only trie check. A trie has exactly one root domain; the Rust side (`KeyTrie::root_width`) panics on a hand-built trie that breaks this.
- The emitter writes the index crate's generated module (key and payload types, typed fold and unfold, prefix functions, payload codec, `decode_payload`). Never edit a generated module by hand; see "regenerate an index module" in [experiments/CLAUDE.md](../experiments/CLAUDE.md).

## Several circuits in one runtime

- Each generated crate exports `<circuit>::SqlCircuit::build`, in a module named after the circuit, so several circuits in one crate do not clash (user decision of 2026-11-09). It sits beside the entry point `circuit`. The Rust runtime side is `Runtime::init_circuits` in `crates/dbsp/src/circuit/dbsp_handle.rs`.
- `SqlCircuit` is an empty enum with `build` as an associated function, so a user function named `build` cannot clash with it.
- The parameter of `build` is `__circuit`, because a SQL-bodied function named `circuit` is declared inside `build` and would shadow a parameter of that name.
- A user function named like any other generated name (`stream12`, `p0`, a Rust UDF `circuit`, and so on) is rejected at `CREATE FUNCTION`. `GeneratedRustNames.java` (package `org.dbsp.sqlCompiler.compiler.backend.rust`) holds the one list; `SqlCircuitConstructorTest` covers it.

## Commands

Use JDK 19–21; the machine default is 17.

```bash
export JAVA_HOME=/opt/homebrew/opt/openjdk@21
cd sql-to-dbsp-compiler/SQL-compiler && mvn package -DskipTests   # rebuild the JAR
cd .. && mvn test -pl SQL-compiler -Dtest='MergedIndex*Test' -Dsurefire.failIfNoSpecifiedTests=false
mvn test -pl SQL-compiler -Dtest=SqlCircuitConstructorTest -Dsurefire.failIfNoSpecifiedTests=false
```

- The full build, which also builds Calcite, is `./build.sh` in `sql-to-dbsp-compiler`.
- Tests that cargo-check the generated crate call plain `cargo`, so also `export RUSTUP_TOOLCHAIN=1.96.1`. Generated Rust lands in `sql-to-dbsp-compiler/temp/`.
- JUnit classes for the spec: `MergedIndexSpecParserTest`, `MergedIndexValidatorTest`, `MergedIndexDerivationTest`, `MergedIndexRustEmitterTest`.
- After changing the emitter, rebuild the JAR and regenerate every index module that depends on it.
