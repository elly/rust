An informal guide to reading and working on the rustc compiler.
==================================================================

If you wish to expand on this document, or have one of the
slightly-more-familiar authors add anything else to it, please get in
touch or file a bug. Your concerns are probably the same as someone
else's.


High-level concepts
===================

Rustc consists of the following subdirectories:

syntax/   - pure syntax concerns: lexer, parser, AST.
front/    - front-end: attributes, conditional compilation
middle/   - middle-end: resolving, typechecking, translating
back/     - back-end: linking and ABI
driver/   - command-line processing, main() entrypoint
util/     - ubiquitous types and helper functions
lib/      - bindings to LLVM
pretty/   - pretty-printing

The entry-point for the compiler is main() in driver/rustc.rs, and
this file sequences the various parts together.


The 3 central data structures:
------------------------------

#1: syntax/ast.rs defines the AST. The AST is treated as immutable
    after parsing despite containing some mutable types (hashtables
    and such).  There are three interesting details to know about this
    structure:

      - Many -- though not all -- nodes within this data structure are
        wrapped in the type spanned<T>, meaning that the front-end has
        marked the input coordinates of that node. The member .node is
        the data itself, the member .span is the input location (file,
        line, column; both low and high).

      - Many other nodes within this data structure carry a
        def_id. These nodes represent the 'target' of some name
        reference elsewhere in the tree. When the AST is resolved, by
        middle/resolve.rs, all names wind up acquiring a def that they
        point to. So anything that can be pointed-to by a name winds
        up with a def_id.

#2: middle/ty.rs defines the datatype sty.  This is the type that
    represents types after they have been resolved and normalized by
    the middle-end. The typeck phase converts every ast type to a
    ty::sty, and the latter is used to drive later phases of
    compilation.  Most variants in the ast::ty tag have a
    corresponding variant in the ty::sty tag.

#3: lib/llvm.rs defines the exported types ValueRef, TypeRef,
    BasicBlockRef, and several others. Each of these is an opaque
    pointer to an LLVM type, manipulated through the lib.llvm
    interface.


Control and information flow within the compiler:
-------------------------------------------------

- main() in driver/rustc.rs assumes control on startup. Options are
  parsed, platform is detected, etc.

- front/parser.rs is driven over the input files.

- Multiple middle-end passes (middle/resolve.rs, middle/typeck.rs) are
  run over the resulting AST. Each pass generates new information
  about the AST which is stored in various side data structures.

- Finally middle/trans.rs is applied to the AST, which performs a
  type-directed translation to LLVM-ese. When it's finished
  synthesizing LLVM values, rustc asks LLVM to write them out in some
  form (.bc, .o) and possibly run the system linker.
