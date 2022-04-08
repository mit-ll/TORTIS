# `transaction` keyword

AST-level changes are mostly in `libsyntax` and `libsyntax_pos`. Search around for `transaction` and you should find my changes.

## Core

`src/libsyntax/ast.rs` adds `TransactionBlock`, `Lock`, and `Unlock` types to the enum.

`src/libsyntax_pos/symbol.rs` adds `kw::Transaction` to the list of symbols.

`src/libsyntax/parse/parser/expr.rs` parses the block if it sees `kw::Transaction`.

## Misc. other files

These are mostly just filling out enums.

`src/libsyntax/mut_visit.rs`

`src/libsyntax/parse/classify.rs`

`src/libsyntax/parse/token.rs`

`src/libsyntax/print/pprust.rs`

`src/libsyntax/util/parser.rs`

`src/libsyntax/visit.rs`

# Lowering `ast::ExprKind::TransactionBlock` to `hir::ExprKind::Lock` and `hir::ExprKind::Unlock`

These changes are mostly in `rustc::hir::lowering`.

`src/librustc/hir/lowering.rs` is the key code that lowers from a `TransactionBlock` to a `Lock` and `Unlock` surrounding a block.

`src/librustc/hir/lowering/expr.rs` also has a bit of glue.

## Misc. other files

These are mostly just filling out enums.

`src/librustc/hir/intravisit.rs`

`src/librustc/hir/mod.rs`

`src/librustc/hir/print.rs`

`src/librustc/middle/expr_use_visitor.rs`

`src/librustc/middle/mem_categorization.rs`

`src/librustc_passes/liveness.rs`

`src/librustc_typeck/check/expr.rs`

# Lowering to HAIR

HAIR is High-level Abstract Intermediate Representation, which is this kind of small middle step between HIR and MIR.

`src/librustc_mir/hair/cx/expr.rs` has the TORTIS implementation.

Most of these changes are in `rustc_mir::transform`, in particular `rustc_mir::transform::transaction`.

`src/librustc/mir/mod.rs` contains new struct definitions so they can be imported across the compiler. Note that `rustc/mir/` and `rustc_mir/` exist at this point in the compiler because the compiler team was in the process of making it more modular. 😛 

# Queries

`src/librustc/query/mod.rs` defines TORTIS's new queries. It's a big macro, so follow the template. See the [rustc dev guide chapter on queries](https://rustc-dev-guide.rust-lang.org/query.html) for more details.

We had to add `#[derive(Eq, PartialEq, Hash)]` to some structs so that they can be used in the query system. The query system needs these properties for memoization.

`src/librustc/mir/interpret/error.rs` has some other misc. structs that needed new `#[derive]`s.

# Emit lock calls

`src/librustc_mir/transform/mod.rs` implements the query functions and applies the patches that insert lock calls. It uses the `rustc_mir::transform::transaction` modules.

`src/librustc_mir/transform/transaction/mod.rs` contains some other helper functions for patching the lock calls.

# Def-use analysis

`src/librustc_mir/transform/transaction/transaction_map.rs` maps function calls to the transactions in which they are contained.

`src/librustc_mir/transform/transaction/use_def_analysis.rs` is the main def-use analysis. It imports `TransactionMap`.

# Conflict analysis

`src/librustc_mir/transform/transaction/conflict_analysis.rs` performs conflict analysis.

# Lang items

`src/librustc/middle/lang_items.rs` is where new lang items are created. It's a big macro, so just follow the template.

# Compiler config

`src/librustc/session/config.rs` lets you set compiler flags. TORTIS creates the `transaction_level` compiler flag. It is later checked at the MIR stage.

Set `transaction_level` in `.cargo/config`.

```bash
$ pwd
/home/ubuntu/dev/transactional-memory/txcell
$ cat .cargo/config
[build]
rustflags = ["--emit", "mir", "-Z", "transaction_level=2"]
```