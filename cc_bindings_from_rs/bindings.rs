// Part of the Crubit project, under the Apache License v2.0 with LLVM
// Exceptions. See /LICENSE for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![feature(rustc_private)]
#![deny(rustc::internal)]

extern crate rustc_attr;
extern crate rustc_hir;
extern crate rustc_infer;
extern crate rustc_middle;
extern crate rustc_span;
extern crate rustc_target;
extern crate rustc_trait_selection;
extern crate rustc_type_ir;

use arc_anyhow::{Context, Error, Result};
use code_gen_utils::{
    escape_non_identifier_chars, format_cc_ident, format_cc_includes, make_rs_ident, CcInclude,
    NamespaceQualifier,
};
use error_report::{anyhow, bail, ensure, ErrorReporting};
use itertools::Itertools;
use proc_macro2::{Ident, Literal, TokenStream};
use quote::{format_ident, quote, ToTokens};
use rustc_attr::find_deprecation;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::{AssocItemKind, Item, ItemKind, Node, Safety, UseKind, UsePath};
use rustc_infer::infer::TyCtxtInferExt;
use rustc_middle::dep_graph::DepContext;
use rustc_middle::mir::Mutability;
use rustc_middle::ty::{self, Ty, TyCtxt}; // See <internal link>/ty.html#import-conventions
use rustc_span::def_id::{DefId, LocalDefId, LOCAL_CRATE};
use rustc_span::symbol::{kw, sym, Symbol};
use rustc_target::abi::{Abi, FieldsShape, Integer, Layout, Primitive, Scalar};
use rustc_target::spec::PanicStrategy;
use rustc_trait_selection::infer::InferCtxtExt;
use rustc_type_ir::RegionKind;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::iter::once;
use std::ops::AddAssign;
use std::rc::Rc;
use std::slice;

memoized::query_group! {
    trait BindingsGenerator<'tcx> {
        /// Compilation context for the crate that the bindings should be generated
        /// for.
        #[input]
        fn tcx(&self) -> TyCtxt<'tcx>;

        /// Format specifier for `#include` Crubit C++ support library headers,
        /// using `{header}` as the place holder.  Example:
        /// `<crubit/support/{header}>` results in `#include
        /// <crubit/support/hdr.h>`.
        #[input]
        fn crubit_support_path_format(&self) -> Rc<str>;

        /// A map from a crate name to the include paths of the corresponding C++
        /// headers This is used when formatting a type exported from another
        /// crate.
        // TODO(b/271857814): A crate name might not be globally unique - the key needs to also cover
        // a "hash" of the crate version and compilation flags.
        #[input]
        fn crate_name_to_include_paths(&self) -> Rc<HashMap<Rc<str>, Vec<CcInclude>>>;

        /// Error collector for generating reports of errors encountered during the generation of bindings.
        #[input]
        fn errors(&self) -> Rc<dyn ErrorReporting>;

        // TODO(b/262878759): Provide a set of enabled/disabled Crubit features.
        #[input]
        fn _features(&self) -> ();

        fn support_header(&self, suffix: &'tcx str) -> CcInclude;

        fn repr_attrs(&self, did: DefId) -> Rc<[rustc_attr::ReprAttr]>;

        fn format_ty_for_cc(
            &self,
            ty: Ty<'tcx>,
            location: TypeLocation,
        ) -> Result<CcSnippet>;

        fn format_default_ctor(
            &self,
            core: Rc<AdtCoreBindings<'tcx>>,
        ) -> Result<ApiSnippets, ApiSnippets>;
        fn format_copy_ctor_and_assignment_operator(
            &self,
            core: Rc<AdtCoreBindings<'tcx>>,
        ) -> Result<ApiSnippets, ApiSnippets>;
        fn format_move_ctor_and_assignment_operator(
            &self,
            core: Rc<AdtCoreBindings<'tcx>>,
        ) -> Result<ApiSnippets, ApiSnippets>;

        fn format_item(&self, def_id: LocalDefId) -> Result<Option<ApiSnippets>>;
        fn format_fn(&self, local_def_id: LocalDefId) -> Result<ApiSnippets>;
        fn format_adt_core(&self, def_id: DefId) -> Result<Rc<AdtCoreBindings<'tcx>>>;
    }
    pub struct Database;
}

fn support_header<'tcx>(db: &dyn BindingsGenerator<'tcx>, suffix: &'tcx str) -> CcInclude {
    CcInclude::support_lib_header(db.crubit_support_path_format(), suffix.into())
}

pub struct Output {
    pub h_body: TokenStream,
    pub rs_body: TokenStream,
}

pub fn generate_bindings(db: &Database) -> Result<Output> {
    let tcx = db.tcx();
    match tcx.sess().panic_strategy() {
        PanicStrategy::Unwind => bail!("No support for panic=unwind strategy (b/254049425)"),
        PanicStrategy::Abort => (),
    };

    let top_comment = {
        let crate_name = tcx.crate_name(LOCAL_CRATE);
        let txt = format!(
            "Automatically @generated C++ bindings for the following Rust crate:\n\
             {crate_name}"
        );
        quote! { __COMMENT__ #txt __NEWLINE__ }
    };

    let Output { h_body, rs_body } = format_crate(db).unwrap_or_else(|err| {
        let txt = format!("Failed to generate bindings for the crate: {err}");
        let src = quote! { __COMMENT__ #txt };
        Output { h_body: src.clone(), rs_body: src }
    });

    let h_body = quote! {
        #top_comment

        // TODO(b/251445877): Replace `#pragma once` with include guards.
        __HASH_TOKEN__ pragma once __NEWLINE__
        __NEWLINE__

        #h_body
    };

    let rs_body = quote! {
        #top_comment

        // `rust_builtin_type_abi_assumptions.md` documents why the generated
        // bindings need to relax the `improper_ctypes_definitions` warning
        // for `char` (and possibly for other built-in types in the future).
        #![allow(improper_ctypes_definitions)] __NEWLINE__

        __NEWLINE__

        #rs_body
    };

    Ok(Output { h_body, rs_body })
}

#[derive(Clone, Debug, Default)]
struct CcPrerequisites {
    /// Set of `#include`s that a `CcSnippet` depends on.  For example if
    /// `CcSnippet::tokens` expands to `std::int32_t`, then `includes`
    /// need to cover the `#include <cstdint>`.
    includes: BTreeSet<CcInclude>,

    /// Set of local definitions that a `CcSnippet` depends on.  For example if
    /// `CcSnippet::tokens` expands to `void foo(S s) { ... }` then the
    /// definition of `S` should have appeared earlier - in this case `defs`
    /// will include the `LocalDefId` corresponding to `S`.  Note that the
    /// definition of `S` is covered by `ApiSnippets::main_api` (i.e. the
    /// predecessor of a toposort edge is `ApiSnippets::main_api` - it is not
    /// possible to depend on `ApiSnippets::cc_details`).
    defs: HashSet<LocalDefId>,

    /// Set of forward declarations that a `CcSnippet` depends on.  For example
    /// if `CcSnippet::tokens` expands to `void foo(S* s)` then a forward
    /// declaration of `S` should have appeared earlier - in this case
    /// `fwd_decls` will include the `LocalDefId` corresponding to `S`.
    /// Note that in this particular example the *definition* of `S` does
    /// *not* need to appear earlier (and therefore `defs` will *not*
    /// contain `LocalDefId` corresponding to `S`).
    fwd_decls: HashSet<LocalDefId>,
}

impl CcPrerequisites {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        let &Self { ref includes, ref defs, ref fwd_decls } = self;
        includes.is_empty() && defs.is_empty() && fwd_decls.is_empty()
    }

    /// Weakens all dependencies to only require a forward declaration. Example
    /// usage scenarios:
    /// - Computing prerequisites of pointer types (the pointee type can just be
    ///   forward-declared),
    /// - Computing prerequisites of function declarations (parameter types and
    ///   return type can just be forward-declared).
    fn move_defs_to_fwd_decls(&mut self) {
        self.fwd_decls.extend(std::mem::take(&mut self.defs))
    }
}

impl AddAssign for CcPrerequisites {
    fn add_assign(&mut self, rhs: Self) {
        let Self { mut includes, defs, fwd_decls } = rhs;

        // `BTreeSet::append` is used because it _seems_ to be more efficient than
        // calling `extend`.  This is because `extend` takes an iterator
        // (processing each `rhs` include one-at-a-time) while `append` steals
        // the whole backing data store from `rhs.includes`. OTOH, this is a bit
        // speculative, since the (expected / guessed) performance difference is
        // not documented at
        // https://doc.rust-lang.org/std/collections/struct.BTreeSet.html#method.append
        self.includes.append(&mut includes);

        self.defs.extend(defs);
        self.fwd_decls.extend(fwd_decls);
    }
}

#[derive(Clone, Debug, Default)]
struct CcSnippet {
    tokens: TokenStream,
    prereqs: CcPrerequisites,
}

impl CcSnippet {
    /// Consumes `self` and returns its `tokens`, while preserving
    /// its `prereqs` into `prereqs_accumulator`.
    fn into_tokens(self, prereqs_accumulator: &mut CcPrerequisites) -> TokenStream {
        let Self { tokens, prereqs } = self;
        *prereqs_accumulator += prereqs;
        tokens
    }

    /// Creates a new CcSnippet (with no `CcPrerequisites`).
    fn new(tokens: TokenStream) -> Self {
        Self { tokens, ..Default::default() }
    }

    /// Creates a CcSnippet that depends on a single `CcInclude`.
    fn with_include(tokens: TokenStream, include: CcInclude) -> Self {
        let mut prereqs = CcPrerequisites::default();
        prereqs.includes.insert(include);
        Self { tokens, prereqs }
    }
}

impl AddAssign for CcSnippet {
    fn add_assign(&mut self, rhs: Self) {
        self.tokens.extend(rhs.into_tokens(&mut self.prereqs));
    }
}

/// Represents the fully qualified name of a Rust item (e.g. of a `struct` or a
/// function).
struct FullyQualifiedName {
    /// Name of the crate that defines the item.
    /// For example, this would be `std` for `std::cmp::Ordering`.
    krate: Symbol,

    /// Path to the module where the item is located.
    /// For example, this would be `cmp` for `std::cmp::Ordering`.
    /// The path may contain multiple modules - e.g. `foo::bar::baz`.
    mod_path: NamespaceQualifier,

    /// Name of the item.
    /// For example, this would be:
    /// * `Some("Ordering")` for `std::cmp::Ordering`.
    /// * `None` for `ItemKind::Use` - e.g.: `use submodule::*`
    name: Option<Symbol>,

    /// The fully-qualified C++ type to use for this, if this was originally a
    /// C++ type.
    ///
    /// For example, if a type has `#[__crubit::annotate(cc_type="x::y")]`, then
    /// cc_type will be `Some(x::y)`.
    cc_type: Option<Symbol>,
}

impl FullyQualifiedName {
    /// Computes a `FullyQualifiedName` for `def_id`.
    ///
    /// May panic if `def_id` is an invalid id.
    // TODO(b/259724276): This function's results should be memoized.
    fn new(tcx: TyCtxt, def_id: DefId) -> Self {
        let krate = tcx.crate_name(def_id.krate);

        // Crash OK: these attributes are introduced by crubit itself, and "should
        // never" be malformed.
        let cc_type = crubit_attr::get(tcx, def_id).unwrap().cc_type;

        let mut full_path = tcx.def_path(def_id).data; // mod_path + name
        let name = full_path.pop().expect("At least the item's name should be present");
        let name = name.data.get_opt_name();

        let mod_path = NamespaceQualifier::new(
            full_path
                .into_iter()
                .filter_map(|p| p.data.get_opt_name())
                .map(|s| Rc::<str>::from(s.as_str())),
        );

        Self { krate, mod_path, name, cc_type }
    }

    fn format_for_cc(&self) -> Result<TokenStream> {
        if let Some(path) = self.cc_type {
            let path = format_cc_ident(path.as_str())?;
            return Ok(quote! {#path});
        }

        let name =
            self.name.as_ref().expect("`format_for_cc` can't be called on name-less item kinds");

        let top_level_ns = format_cc_ident(self.krate.as_str())?;
        let ns_path = self.mod_path.format_for_cc()?;
        let name = format_cc_ident(name.as_str())?;
        Ok(quote! { :: #top_level_ns :: #ns_path #name })
    }

    fn format_for_rs(&self) -> TokenStream {
        let name =
            self.name.as_ref().expect("`format_for_rs` can't be called on name-less item kinds");

        let krate = make_rs_ident(self.krate.as_str());
        let mod_path = self.mod_path.format_for_rs();
        let name = make_rs_ident(name.as_str());
        quote! { :: #krate :: #mod_path #name }
    }
}

/// Whether functions using `extern "C"` ABI can safely handle values of type
/// `ty` (e.g. when passing by value arguments or return values of such type).
fn is_c_abi_compatible_by_value(ty: Ty) -> bool {
    match ty.kind() {
        // `improper_ctypes_definitions` warning doesn't complain about the following types:
        ty::TyKind::Bool |
        ty::TyKind::Float{..} |
        ty::TyKind::Int{..} |
        ty::TyKind::Uint{..} |
        ty::TyKind::Never |
        ty::TyKind::RawPtr{..} |
        ty::TyKind::Ref{..} |
        ty::TyKind::FnPtr{..} => true,
        ty::TyKind::Tuple(types) if types.len() == 0 => true,

        // Crubit assumes that `char` is compatible with a certain `extern "C"` ABI.
        // See `rust_builtin_type_abi_assumptions.md` for more details.
        ty::TyKind::Char => true,

        // Crubit's C++ bindings for tuples, structs, and other ADTs may not preserve
        // their ABI (even if they *do* preserve their memory layout).  For example:
        // - In System V ABI replacing a field with a fixed-length array of bytes may affect
        //   whether the whole struct is classified as an integer and passed in general purpose
        //   registers VS classified as SSE2 and passed in floating-point registers like xmm0).
        //   See also b/270454629.
        // - To replicate field offsets, Crubit may insert explicit padding fields. These
        //   extra fields may also impact the ABI of the generated bindings.
        //
        // TODO(lukasza): In the future, some additional performance gains may be realized by
        // returning `true` in a few limited cases (this may require additional complexity to
        // ensure that `format_adt` never injects explicit padding into such structs):
        // - `#[repr(C)]` structs and unions,
        // - `#[repr(transparent)]` struct that wraps an ABI-safe type,
        // - Discriminant-only enums (b/259984090).
        ty::TyKind::Tuple{..} |  // An empty tuple (`()` - the unit type) is handled above.
        ty::TyKind::Adt{..} => false,

        // These kinds of reference-related types are not implemented yet - `is_c_abi_compatible_by_value`
        // should never need to handle them, because `format_ty_for_cc` fails for such types.
        //
        // TODO(b/258235219): When implementing support for references we should
        // consider returning `true` for `TyKind::Ref` and document the rationale
        // for such decision - maybe something like this will be sufficient:
        // - In general `TyKind::Ref` should have the same ABI as `TyKind::RawPtr`
        // - References to slices (`&[T]`) or strings (`&str`) rely on assumptions
        //   spelled out in `rust_builtin_type_abi_assumptions.md`..
        ty::TyKind::Str |
        ty::TyKind::Array{..} |
        ty::TyKind::Slice{..} =>
            unimplemented!(),

        // `format_ty_for_cc` is expected to fail for other kinds of types
        // and therefore `is_c_abi_compatible_by_value` should never be called for
        // these other types
        _ => unimplemented!(),
    }
}

/// Location where a type is used.
#[derive(PartialEq, Eq, Hash, Copy, Clone, Debug)]
enum TypeLocation {
    /// The top-level return type.
    ///
    /// The "top-level" part can be explained by looking at an example of `fn
    /// foo() -> *const T`:
    /// - The top-level return type `*const T` is in the `FnReturn` location
    /// - The nested pointee type `T` is in the `Other` location
    FnReturn,

    /// The top-level parameter type.
    ///
    /// The "top-level" part can be explained by looking at an example of:
    /// `fn foo(param: *const T)`:
    /// - The top-level parameter type `*const T` is in the `FnParam` location
    /// - The nested pointee type `T` is in the `Other` location
    // TODO(b/278141494, b/278141418): Once `const` and `static` items are supported,
    // we may want to apply parameter-like formatting to their types (e.g. have
    // `format_ty_for_cc` emit `T&` rather than `T*`).
    FnParam,

    /// Other location (e.g. pointee type, field type, etc.).
    Other,
}

fn format_pointer_or_reference_ty_for_cc<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    pointee: Ty<'tcx>,
    mutability: rustc_middle::mir::Mutability,
    pointer_sigil: TokenStream,
) -> Result<CcSnippet> {
    let tcx = db.tcx();
    let const_qualifier = match mutability {
        Mutability::Mut => quote! {},
        Mutability::Not => quote! { const },
    };
    if pointee.is_c_void(tcx) {
        return Ok(CcSnippet { tokens: quote! { #const_qualifier void* }, ..Default::default() });
    }
    let CcSnippet { tokens, mut prereqs } = db.format_ty_for_cc(pointee, TypeLocation::Other)?;
    prereqs.move_defs_to_fwd_decls();
    Ok(CcSnippet { prereqs, tokens: quote! { #tokens #const_qualifier #pointer_sigil } })
}

/// Formats `ty` into a `CcSnippet` that represents how the type should be
/// spelled in a C++ declaration of a function parameter or field.
fn format_ty_for_cc<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    ty: Ty<'tcx>,
    location: TypeLocation,
) -> Result<CcSnippet> {
    let tcx = db.tcx();
    fn cstdint(tokens: TokenStream) -> CcSnippet {
        CcSnippet::with_include(tokens, CcInclude::cstdint())
    }
    fn keyword(tokens: TokenStream) -> CcSnippet {
        CcSnippet::new(tokens)
    }
    Ok(match ty.kind() {
        ty::TyKind::Never => match location {
            TypeLocation::FnReturn => keyword(quote! { void }),
            _ => {
                // TODO(b/254507801): Maybe translate into `crubit::Never`?
                bail!("The never type `!` is only supported as a return type (b/254507801)");
            }
        },
        ty::TyKind::Tuple(types) => {
            if types.len() == 0 {
                match location {
                    TypeLocation::FnReturn => keyword(quote! { void }),
                    _ => {
                        // TODO(b/254507801): Maybe translate into `crubit::Unit`?
                        bail!("`()` / `void` is only supported as a return type (b/254507801)");
                    }
                }
            } else {
                // TODO(b/254099023): Add support for tuples.
                bail!("Tuples are not supported yet: {} (b/254099023)", ty);
            }
        }

        // https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#bool documents
        // that "Rust's bool has the same layout as C17's _Bool".  The details (e.g. size, valid
        // bit patterns) are implementation-defined, but this is okay, because `bool` in the
        // `extern "C"` functions in the generated `..._cc_api.h` will also be the C17's _Bool.
        ty::TyKind::Bool => keyword(quote! { bool }),

        // https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#fixed-width-floating-point-types
        // documents that "When the platforms' "math.h" header defines the __STDC_IEC_559__ macro,
        // Rust's floating-point types are safe to use directly in C FFI where the appropriate C
        // types are expected (f32 for float, f64 for double)."
        //
        // TODO(b/255768062): Generated bindings should explicitly check `__STDC_IEC_559__`
        ty::TyKind::Float(ty::FloatTy::F32) => keyword(quote! { float }),
        ty::TyKind::Float(ty::FloatTy::F64) => keyword(quote! { double }),

        // ABI compatibility and other details are described in the doc comments in
        // `crubit/support/rs_std/rs_char.h` and `crubit/support/rs_std/char_test.cc` (search for
        // "Layout tests").
        ty::TyKind::Char => {
            // Asserting that the target architecture meets the assumption from Crubit's
            // `rust_builtin_type_abi_assumptions.md` - we assume that Rust's `char` has the
            // same ABI as `u32`.
            let layout = tcx
                .layout_of(ty::ParamEnv::empty().and(ty))
                .expect("`layout_of` is expected to succeed for the builtin `char` type")
                .layout;
            assert_eq!(4, layout.align().abi.bytes());
            assert_eq!(4, layout.size().bytes());
            assert!(matches!(
                layout.abi(),
                Abi::Scalar(Scalar::Initialized {
                    value: Primitive::Int(Integer::I32, /* signedness = */ false),
                    ..
                })
            ));

            CcSnippet::with_include(
                quote! { rs_std::rs_char },
                db.support_header("rs_std/rs_char.h"),
            )
        }

        // https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#isize-and-usize
        // documents that "Rust's signed and unsigned fixed-width integer types {i,u}{8,16,32,64}
        // have the same layout the C fixed-width integer types from the <stdint.h> header
        // {u,}int{8,16,32,64}_t. These fixed-width integer types are therefore safe to use
        // directly in C FFI where the corresponding C fixed-width integer types are expected.
        //
        // https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#layout-compatibility-with-c-native-integer-types
        // documents that "Rust does not support C platforms on which the C native integer type are
        // not compatible with any of Rust's fixed-width integer type (e.g. because of
        // padding-bits, lack of 2's complement, etc.)."
        ty::TyKind::Int(ty::IntTy::I8) => cstdint(quote! { std::int8_t }),
        ty::TyKind::Int(ty::IntTy::I16) => cstdint(quote! { std::int16_t }),
        ty::TyKind::Int(ty::IntTy::I32) => cstdint(quote! { std::int32_t }),
        ty::TyKind::Int(ty::IntTy::I64) => cstdint(quote! { std::int64_t }),
        ty::TyKind::Uint(ty::UintTy::U8) => cstdint(quote! { std::uint8_t }),
        ty::TyKind::Uint(ty::UintTy::U16) => cstdint(quote! { std::uint16_t }),
        ty::TyKind::Uint(ty::UintTy::U32) => cstdint(quote! { std::uint32_t }),
        ty::TyKind::Uint(ty::UintTy::U64) => cstdint(quote! { std::uint64_t }),

        // https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#isize-and-usize
        // documents that "The isize and usize types are [...] layout compatible with C's uintptr_t
        // and intptr_t types.".
        ty::TyKind::Int(ty::IntTy::Isize) => cstdint(quote! { std::intptr_t }),
        ty::TyKind::Uint(ty::UintTy::Usize) => cstdint(quote! { std::uintptr_t }),

        ty::TyKind::Int(ty::IntTy::I128) | ty::TyKind::Uint(ty::UintTy::U128) => {
            // Note that "the alignment of Rust's {i,u}128 is unspecified and allowed to
            // change" according to
            // https://rust-lang.github.io/unsafe-code-guidelines/layout/scalars.html#fixed-width-integer-types
            //
            // TODO(b/254094650): Consider mapping this to Clang's (and GCC's) `__int128`
            // or to `absl::in128`.
            bail!("C++ doesn't have a standard equivalent of `{ty}` (b/254094650)");
        }

        ty::TyKind::Adt(adt, substs) => {
            ensure!(substs.len() == 0, "Generic types are not supported yet (b/259749095)");
            ensure!(
                is_directly_public(tcx, adt.did()),
                "Not directly public type (re-exports are not supported yet - b/262052635)"
            );

            let def_id = adt.did();
            let mut prereqs = CcPrerequisites::default();
            if def_id.krate == LOCAL_CRATE {
                prereqs.defs.insert(def_id.expect_local());
            } else {
                let other_crate_name = tcx.crate_name(def_id.krate);
                let crate_name_to_include_paths = db.crate_name_to_include_paths();
                let includes = crate_name_to_include_paths
                    .get(other_crate_name.as_str())
                    .ok_or_else(|| {
                        anyhow!(
                            "Type `{ty}` comes from the `{other_crate_name}` crate, \
                             but no `--bindings-from-dependency` was specified for this crate"
                        )
                    })?;
                prereqs.includes.extend(includes.iter().cloned());
            }

            // Verify if definition of `ty` can be succesfully imported and bail otherwise.
            db.format_adt_core(def_id).with_context(|| {
                format!("Failed to generate bindings for the definition of `{ty}`")
            })?;

            CcSnippet { tokens: FullyQualifiedName::new(tcx, def_id).format_for_cc()?, prereqs }
        }

        ty::TyKind::RawPtr(pointee_ty, mutbl) => {
            format_pointer_or_reference_ty_for_cc(db, *pointee_ty, *mutbl, quote! { * })
                .with_context(|| {
                    format!("Failed to format the pointee of the pointer type `{ty}`")
                })?
        }

        ty::TyKind::Ref(region, referent_ty, mutability) => {
            match location {
                TypeLocation::FnReturn | TypeLocation::FnParam => (),
                TypeLocation::Other => bail!(
                    "Can't format `{ty}`, because references are only supported in \
                     function parameter types and return types (b/286256327)",
                ),
            };
            let lifetime = format_region_as_cc_lifetime(region);
            format_pointer_or_reference_ty_for_cc(
                db,
                *referent_ty,
                *mutability,
                quote! { & #lifetime },
            )
            .with_context(|| {
                format!("Failed to format the referent of the reference type `{ty}`")
            })?
        }

        ty::TyKind::FnPtr(sig) => {
            let sig = match sig.no_bound_vars() {
                None => bail!("Generic functions are not supported yet (b/259749023)"),
                Some(sig) => sig,
            };
            check_fn_sig(&sig)?;
            is_thunk_required(&sig).context("Function pointers can't have a thunk")?;

            // `is_thunk_required` check above implies `extern "C"` (or `"C-unwind"`).
            // This assertion reinforces that the generated C++ code doesn't need
            // to use calling convention attributes like `_stdcall`, etc.
            assert!(matches!(sig.abi, rustc_target::spec::abi::Abi::C { .. }));

            // C++ references are not rebindable and therefore can't be used to replicate
            // semantics of Rust field types (or, say, element types of Rust
            // arrays).  Because of this, C++ references are only used for
            // top-level return types and parameter types (and pointers are used
            // in other locations).
            let ptr_or_ref_sigil = match location {
                TypeLocation::FnReturn | TypeLocation::FnParam => quote! { & },
                TypeLocation::Other => quote! { * },
            };

            let mut prereqs = CcPrerequisites::default();
            prereqs.includes.insert(db.support_header("internal/cxx20_backports.h"));
            let ret_type = format_ret_ty_for_cc(db, &sig)?.into_tokens(&mut prereqs);
            let param_types = format_param_types_for_cc(db, &sig)?
                .into_iter()
                .map(|snippet| snippet.into_tokens(&mut prereqs));
            let tokens = quote! {
                crubit::type_identity_t<
                    #ret_type( #( #param_types ),* )
                > #ptr_or_ref_sigil
            };

            CcSnippet { tokens, prereqs }
        }

        // TODO(b/260268230, b/260729464): When recursively processing nested types (e.g. an
        // element type of an Array, a referent of a Ref, a parameter type of an FnPtr, etc), one
        // should also 1) propagate `CcPrerequisites::defs`, 2) cover `CcPrerequisites::defs` in
        // `test_format_ty_for_cc...`.  For ptr/ref it might be possible to use
        // `CcPrerequisites::move_defs_to_fwd_decls`.
        _ => bail!("The following Rust type is not supported yet: {ty}"),
    })
}

fn format_ret_ty_for_cc<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    sig: &ty::FnSig<'tcx>,
) -> Result<CcSnippet> {
    db.format_ty_for_cc(sig.output(), TypeLocation::FnReturn)
        .context("Error formatting function return type")
}

fn format_param_types_for_cc<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    sig: &ty::FnSig<'tcx>,
) -> Result<Vec<CcSnippet>> {
    sig.inputs()
        .iter()
        .enumerate()
        .map(|(i, &ty)| {
            db.format_ty_for_cc(ty, TypeLocation::FnParam)
                .with_context(|| format!("Error handling parameter #{i}"))
        })
        .collect()
}

/// Formats `ty` for Rust - to be used in `..._cc_api_impl.rs` (e.g. as a type
/// of a parameter in a Rust thunk).  Because `..._cc_api_impl.rs` is a
/// distinct, separate crate, the returned `TokenStream` uses crate-qualified
/// names whenever necessary - for example: `target_crate::SomeStruct` rather
/// than just `SomeStruct`.
//
// TODO(b/259724276): This function's results should be memoized.
fn format_ty_for_rs(tcx: TyCtxt, ty: Ty) -> Result<TokenStream> {
    Ok(match ty.kind() {
        ty::TyKind::Bool
        | ty::TyKind::Float(_)
        | ty::TyKind::Char
        | ty::TyKind::Int(_)
        | ty::TyKind::Uint(_)
        | ty::TyKind::FnPtr(_)
        | ty::TyKind::Never => ty
            .to_string()
            .parse()
            .expect("rustc_middle::ty::Ty::to_string() should produce no parsing errors"),
        ty::TyKind::Tuple(types) => {
            if types.len() == 0 {
                quote! { () }
            } else {
                // TODO(b/254099023): Add support for tuples.
                bail!("Tuples are not supported yet: {} (b/254099023)", ty);
            }
        }
        ty::TyKind::Adt(adt, substs) => {
            ensure!(substs.len() == 0, "Generic types are not supported yet (b/259749095)");
            FullyQualifiedName::new(tcx, adt.did()).format_for_rs()
        }
        ty::TyKind::RawPtr(pointee_ty, mutbl) => {
            let qualifier = match mutbl {
                Mutability::Mut => quote! { mut },
                Mutability::Not => quote! { const },
            };
            let ty = format_ty_for_rs(tcx, *pointee_ty).with_context(|| {
                format!("Failed to format the pointee of the pointer type `{ty}`")
            })?;
            quote! { * #qualifier #ty }
        }
        ty::TyKind::Ref(region, referent_ty, mutability) => {
            let mutability = match mutability {
                Mutability::Mut => quote! { mut },
                Mutability::Not => quote! {},
            };
            let ty = format_ty_for_rs(tcx, *referent_ty).with_context(|| {
                format!("Failed to format the referent of the reference type `{ty}`")
            })?;
            let lifetime = format_region_as_rs_lifetime(region);
            quote! { & #lifetime #mutability #ty }
        }
        _ => bail!("The following Rust type is not supported yet: {ty}"),
    })
}

fn format_region_as_cc_lifetime(region: &ty::Region) -> TokenStream {
    let name =
        region.get_name().expect("Caller should use `liberate_and_deanonymize_late_bound_regions`");
    let name = name
        .as_str()
        .strip_prefix('\'')
        .expect("All Rust lifetimes are expected to begin with the \"'\" character");

    // TODO(b/286299326): Use `$a` or `$(foo)` or `$static` syntax below.
    quote! { [[clang::annotate_type("lifetime", #name)]] }
}

fn format_region_as_rs_lifetime(region: &ty::Region) -> TokenStream {
    let name =
        region.get_name().expect("Caller should use `liberate_and_deanonymize_late_bound_regions`");
    let lifetime = syn::Lifetime::new(name.as_str(), proc_macro2::Span::call_site());
    quote! { #lifetime }
}

#[derive(Clone, Debug, Default)]
struct ApiSnippets {
    /// Main API - for example:
    /// - A C++ declaration of a function (with a doc comment),
    /// - A C++ definition of a struct (with a doc comment).
    main_api: CcSnippet,

    /// C++ implementation details - for example:
    /// - A C++ declaration of an `extern "C"` thunk,
    /// - C++ `static_assert`s about struct size, aligment, and field offsets.
    cc_details: CcSnippet,

    /// Rust implementation details - for exmaple:
    /// - A Rust implementation of an `extern "C"` thunk,
    /// - Rust `assert!`s about struct size, aligment, and field offsets.
    rs_details: TokenStream,
}

impl FromIterator<ApiSnippets> for ApiSnippets {
    fn from_iter<I: IntoIterator<Item = ApiSnippets>>(iter: I) -> Self {
        let mut result = ApiSnippets::default();
        for ApiSnippets { main_api, cc_details, rs_details } in iter.into_iter() {
            result.main_api += main_api;
            result.cc_details += cc_details;
            result.rs_details.extend(rs_details);
        }
        result
    }
}

/// Similar to `TyCtxt::liberate_and_name_late_bound_regions` but also replaces
/// anonymous regions with new names.
fn liberate_and_deanonymize_late_bound_regions<'tcx>(
    tcx: TyCtxt<'tcx>,
    sig: ty::PolyFnSig<'tcx>,
    fn_def_id: DefId,
) -> ty::FnSig<'tcx> {
    let mut anon_count: u32 = 0;
    let mut translated_kinds: HashMap<ty::BoundVar, ty::BoundRegionKind> = HashMap::new();
    let region_f = |br: ty::BoundRegion| {
        let new_kind: &ty::BoundRegionKind = translated_kinds.entry(br.var).or_insert_with(|| {
            let name = br.kind.get_name().unwrap_or_else(|| {
                anon_count += 1;
                Symbol::intern(&format!("'__anon{anon_count}"))
            });
            let id = br.kind.get_id().unwrap_or(fn_def_id);
            ty::BoundRegionKind::BrNamed(id, name)
        });
        ty::Region::new_late_param(tcx, fn_def_id, *new_kind)
    };
    tcx.instantiate_bound_regions_uncached(sig, region_f)
}

fn get_fn_sig(tcx: TyCtxt, fn_def_id: LocalDefId) -> ty::FnSig {
    let fn_def_id = fn_def_id.to_def_id(); // LocalDefId => DefId
    let sig = tcx.fn_sig(fn_def_id).instantiate_identity();
    liberate_and_deanonymize_late_bound_regions(tcx, sig, fn_def_id)
}

/// Formats a C++ function declaration of a thunk that wraps a Rust function
/// identified by `fn_def_id`.  `format_thunk_impl` may panic if `fn_def_id`
/// doesn't identify a function.
fn format_thunk_decl<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    fn_def_id: DefId,
    sig: &ty::FnSig<'tcx>,
    thunk_name: &TokenStream,
) -> Result<CcSnippet> {
    let tcx = db.tcx();

    let mut prereqs = CcPrerequisites::default();
    let main_api_ret_type = format_ret_ty_for_cc(db, sig)?.into_tokens(&mut prereqs);

    let mut thunk_params = {
        let cc_types = format_param_types_for_cc(db, sig)?;
        sig.inputs()
            .iter()
            .zip(cc_types.into_iter())
            .map(|(&ty, cc_type)| -> Result<TokenStream> {
                let cc_type = cc_type.into_tokens(&mut prereqs);
                if is_c_abi_compatible_by_value(ty) {
                    Ok(quote! { #cc_type })
                } else {
                    // Rust thunk will move a value via memcpy - we need to `ensure` that
                    // invoking the C++ destructor (on the moved-away value) is safe.
                    ensure!(
                        !ty.needs_drop(tcx, tcx.param_env(fn_def_id)),
                        "Only trivially-movable and trivially-destructible types \
                              may be passed by value over the FFI boundary"
                    );
                    Ok(quote! { #cc_type* })
                }
            })
            .collect::<Result<Vec<_>>>()?
    };

    let thunk_ret_type: TokenStream;
    if is_c_abi_compatible_by_value(sig.output()) {
        thunk_ret_type = main_api_ret_type;
    } else {
        thunk_ret_type = quote! { void };
        thunk_params.push(quote! { #main_api_ret_type* __ret_ptr });
    };
    Ok(CcSnippet {
        prereqs,
        tokens: quote! {
            namespace __crubit_internal {
                extern "C" #thunk_ret_type #thunk_name ( #( #thunk_params ),* );
            }
        },
    })
}

/// Formats a thunk implementation in Rust that provides an `extern "C"` ABI for
/// calling a Rust function identified by `fn_def_id`.  `format_thunk_impl` may
/// panic if `fn_def_id` doesn't identify a function.
///
/// `fully_qualified_fn_name` specifies how the thunk can identify the function
/// to call. Examples of valid arguments:
/// - `::crate_name::some_module::free_function`
/// - `::crate_name::some_module::SomeStruct::method`
/// - `<::crate_name::some_module::SomeStruct as
///   ::core::default::Default>::default`
fn format_thunk_impl<'tcx>(
    tcx: TyCtxt<'tcx>,
    fn_def_id: DefId,
    sig: &ty::FnSig<'tcx>,
    thunk_name: &str,
    fully_qualified_fn_name: TokenStream,
) -> Result<TokenStream> {
    let param_names_and_types: Vec<(Ident, Ty)> = {
        let param_names = tcx.fn_arg_names(fn_def_id).iter().enumerate().map(|(i, ident)| {
            if ident.as_str().is_empty() {
                format_ident!("__param_{i}")
            } else if ident.name == kw::SelfLower {
                format_ident!("__self")
            } else {
                make_rs_ident(ident.as_str())
            }
        });
        let param_types = sig.inputs().iter().copied();
        param_names.zip(param_types).collect_vec()
    };

    let mut thunk_params = param_names_and_types
        .iter()
        .map(|(param_name, ty)| {
            let rs_type = format_ty_for_rs(tcx, *ty)
                .with_context(|| format!("Error handling parameter `{param_name}`"))?;
            Ok(if is_c_abi_compatible_by_value(*ty) {
                quote! { #param_name: #rs_type }
            } else {
                quote! { #param_name: &mut ::core::mem::MaybeUninit<#rs_type> }
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut thunk_ret_type = format_ty_for_rs(tcx, sig.output())?;
    let mut thunk_body = {
        let fn_args = param_names_and_types.iter().map(|(rs_name, ty)| {
            if is_c_abi_compatible_by_value(*ty) {
                quote! { #rs_name }
            } else if let Safety::Unsafe = sig.safety {
                // The whole call will be wrapped in `unsafe` below.
                quote! { #rs_name.assume_init_read() }
            } else {
                quote! { unsafe { #rs_name.assume_init_read() } }
            }
        });
        quote! {
            #fully_qualified_fn_name( #( #fn_args ),* )
        }
    };
    // Wrap the call in an unsafe block, for the sake of RFC #2585
    // `unsafe_block_in_unsafe_fn`.
    if let Safety::Unsafe = sig.safety {
        thunk_body = quote! {unsafe {#thunk_body}};
    }
    if !is_c_abi_compatible_by_value(sig.output()) {
        thunk_params.push(quote! {
            __ret_slot: &mut ::core::mem::MaybeUninit<#thunk_ret_type>
        });
        thunk_ret_type = quote! { () };
        thunk_body = quote! { __ret_slot.write(#thunk_body); };
    };

    let generic_params = {
        let regions = sig
            .inputs()
            .iter()
            .copied()
            .chain(std::iter::once(sig.output()))
            .flat_map(|ty| {
                ty.walk().filter_map(|generic_arg| match generic_arg.unpack() {
                    ty::GenericArgKind::Const(_) | ty::GenericArgKind::Type(_) => None,
                    ty::GenericArgKind::Lifetime(region) => Some(region),
                })
            })
            .filter(|region| match region.kind() {
                RegionKind::ReStatic => false,
                RegionKind::ReLateParam(_) => true,
                _ => panic!("Unexpected region kind: {region}"),
            })
            .sorted_by_key(|region| {
                region
                    .get_name()
                    .expect("Caller should use `liberate_and_deanonymize_late_bound_regions`")
            })
            .dedup()
            .collect_vec();
        if regions.is_empty() {
            quote! {}
        } else {
            let lifetimes = regions.into_iter().map(|region| format_region_as_rs_lifetime(&region));
            quote! { < #( #lifetimes ),* > }
        }
    };

    let thunk_name = make_rs_ident(thunk_name);
    let unsafe_qualifier = if let Safety::Unsafe = sig.safety {
        quote! {unsafe}
    } else {
        quote! {}
    };
    Ok(quote! {
        #[no_mangle]
        #unsafe_qualifier extern "C" fn #thunk_name #generic_params (
            #( #thunk_params ),*
        ) -> #thunk_ret_type {
            #thunk_body
        }
    })
}

fn check_fn_sig(sig: &ty::FnSig) -> Result<()> {
    if sig.c_variadic {
        // TODO(b/254097223): Add support for variadic functions.
        bail!("C variadic functions are not supported (b/254097223)");
    }

    Ok(())
}

/// Returns `Ok(())` if no thunk is required.
/// Otherwise returns an error the describes why the thunk is needed.
fn is_thunk_required(sig: &ty::FnSig) -> Result<()> {
    match sig.abi {
        // "C" ABI is okay: Before https://rust-lang.github.io/rfcs/2945-c-unwind-abi.html a
        // Rust panic that "escapes" a "C" ABI function leads to Undefined Behavior.  This is
        // unfortunate, but Crubit's `panics_and_exceptions.md` documents that `-Cpanic=abort`
        // is the only supported configuration.
        //
        // After https://rust-lang.github.io/rfcs/2945-c-unwind-abi.html a Rust panic that
        // tries to "escape" a "C" ABI function will terminate the program.  This is okay.
        rustc_target::spec::abi::Abi::C { unwind: false } => (),

        // "C-unwind" ABI is okay: After
        // https://rust-lang.github.io/rfcs/2945-c-unwind-abi.html a new "C-unwind" ABI may be
        // used by Rust functions that want to safely propagate Rust panics through frames that
        // may belong to another language.
        rustc_target::spec::abi::Abi::C { unwind: true } => (),

        // All other ABIs trigger thunk generation.  This covers Rust ABI functions, but also
        // ABIs that theoretically are understood both by C++ and Rust (e.g. see
        // `format_cc_call_conv_as_clang_attribute` in `rs_bindings_from_cc/src_code_gen.rs`).
        _ => bail!("Calling convention other than `extern \"C\"` requires a thunk"),
    };

    ensure!(is_c_abi_compatible_by_value(sig.output()), "Return type requires a thunk");
    for (i, param_ty) in sig.inputs().iter().enumerate() {
        ensure!(is_c_abi_compatible_by_value(*param_ty), "Type of parameter #{i} requires a thunk");
    }

    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum FunctionKind {
    /// Free function (i.e. not a method).
    Free,

    /// Static method (i.e. the first parameter is not named `self`).
    StaticMethod,

    /// Instance method taking `self` by value (i.e. `self: Self`).
    MethodTakingSelfByValue,

    /// Instance method taking `self` by reference (i.e. `&self` or `&mut
    /// self`).
    MethodTakingSelfByRef,
}

impl FunctionKind {
    fn has_self_param(&self) -> bool {
        match self {
            FunctionKind::MethodTakingSelfByValue | FunctionKind::MethodTakingSelfByRef => true,
            FunctionKind::Free | FunctionKind::StaticMethod => false,
        }
    }
}

/// Checks if the item associated with the given def_id has a deprecated
/// attribute. If so, returns the corresponding C++ deprecated tag.
///
/// TODO(codyheiner): consider adding a more general version of this function
/// that builds a Vec<TokenStream> containing all the attributes of a given
/// item.
fn format_deprecated_tag(tcx: TyCtxt, def_id: DefId) -> Option<TokenStream> {
    if let Some(deprecated_attr) = tcx.get_attr(def_id, rustc_span::symbol::sym::deprecated) {
        if let Some((deprecation, _span)) =
            find_deprecation(tcx.sess(), tcx.features(), slice::from_ref(deprecated_attr))
        {
            let cc_deprecated_tag = match deprecation.note {
                None => quote! {[[deprecated]]},
                Some(note_symbol) => {
                    let note = note_symbol.as_str();
                    quote! {[[deprecated(#note)]]}
                }
            };
            return Some(cc_deprecated_tag);
        }
    }
    None
}

fn format_use(
    db: &dyn BindingsGenerator<'_>,
    using_name: &str,
    use_path: &UsePath,
    use_kind: &UseKind,
) -> Result<ApiSnippets> {
    let tcx = db.tcx();

    // TODO(b/350772554): Support multiple items with the same name in `use`
    // statements.`
    if use_path.res.len() != 1 {
        bail!(
            "use statements which resolve to multiple items with the same name are not supported yet"
        );
    }

    match use_kind {
        UseKind::Single => {}
        // TODO(b/350772554): Implement `pub use foo::{x,y}` and `pub use foo::*`
        UseKind::Glob | UseKind::ListStem => {
            bail!("Unsupported use kind: {use_kind:?}");
        }
    };
    let (def_kind, def_id) = match use_path.res[0] {
        // TODO(b/350772554): Support PrimTy.
        Res::Def(def_kind, def_id) => (def_kind, def_id),
        _ => {
            bail!(
                "Unsupported use statement that refers to this type of the entity: {:#?}",
                use_path.res[0]
            );
        }
    };
    ensure!(
        is_directly_public(tcx, def_id),
        "Not directly public type (re-exports are not supported yet - b/262052635)"
    );

    match def_kind {
        DefKind::Fn => {
            let mut prereqs;
            // TODO(b/350772554): Support exporting private functions.
            if let Some(local_id) = def_id.as_local() {
                if let Ok(snippet) = db.format_fn(local_id) {
                    prereqs = snippet.main_api.prereqs;
                } else {
                    bail!("Ignoring the use because the bindings for the target is not generated");
                }
            } else {
                bail!("Unsupported checking for external function");
            }
            let fully_qualified_fn_name = FullyQualifiedName::new(tcx, def_id);
            let unqualified_rust_fn_name =
                fully_qualified_fn_name.name.expect("Functions are assumed to always have a name");
            let formatted_fully_qualified_fn_name = fully_qualified_fn_name.format_for_cc()?;
            let cpp_name = crubit_attr::get(tcx, def_id).unwrap().cpp_name;
            let main_api_fn_name =
                format_cc_ident(cpp_name.unwrap_or(unqualified_rust_fn_name).as_str())
                    .context("Error formatting function name")?;
            let using_name = format_cc_ident(using_name).context("Error formatting using name")?;

            prereqs.defs.insert(def_id.expect_local());
            let tokens = if format!("{}", using_name) == format!("{}", main_api_fn_name) {
                quote! {using #formatted_fully_qualified_fn_name;}
            } else {
                // TODO(b/350772554): Support function alias.
                bail!("Unsupported function alias");
            };
            Ok(ApiSnippets {
                main_api: CcSnippet { prereqs, tokens },
                cc_details: CcSnippet::default(),
                rs_details: quote! {},
            })
        }
        DefKind::Struct | DefKind::Enum => {
            let use_type = tcx.type_of(def_id).instantiate_identity();
            create_type_alias(db, using_name, use_type)
        }
        _ => bail!(
            "Unsupported use statement that refers to this type of the entity: {:#?}",
            use_path.res
        ),
    }
}

fn format_type_alias(
    db: &dyn BindingsGenerator<'_>,
    local_def_id: LocalDefId,
) -> Result<ApiSnippets> {
    let tcx = db.tcx();
    let def_id: DefId = local_def_id.to_def_id();
    let alias_type = tcx.type_of(def_id).instantiate_identity();
    create_type_alias(db, tcx.item_name(def_id).as_str(), alias_type)
}

fn create_type_alias<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    alias_name: &str,
    alias_type: Ty<'tcx>,
) -> Result<ApiSnippets> {
    let cc_bindings = format_ty_for_cc(db, alias_type, TypeLocation::Other)?;
    let mut main_api_prereqs = CcPrerequisites::default();
    let actual_type_name = cc_bindings.into_tokens(&mut main_api_prereqs);

    let alias_name = format_cc_ident(alias_name).context("Error formatting type alias name")?;
    let tokens = quote! {using #alias_name = #actual_type_name;};

    Ok(ApiSnippets {
        main_api: CcSnippet { prereqs: main_api_prereqs, tokens },
        cc_details: CcSnippet::default(),
        rs_details: quote! {},
    })
}

/// Formats a function with the given `local_def_id`.
///
/// Will panic if `local_def_id`
/// - is invalid
/// - doesn't identify a function,
fn format_fn(db: &dyn BindingsGenerator<'_>, local_def_id: LocalDefId) -> Result<ApiSnippets> {
    let tcx = db.tcx();
    let def_id: DefId = local_def_id.to_def_id(); // Convert LocalDefId to DefId.

    ensure!(
        tcx.generics_of(def_id).count() == 0,
        "Generic functions are not supported yet (b/259749023)"
    );

    let sig = get_fn_sig(tcx, local_def_id);
    check_fn_sig(&sig)?;
    // TODO(b/262904507): Don't require thunks for mangled extern "C" functions.
    let needs_thunk = is_thunk_required(&sig).is_err()
        || (tcx.get_attr(def_id, rustc_span::symbol::sym::no_mangle).is_none()
            && tcx.get_attr(def_id, rustc_span::symbol::sym::export_name).is_none());
    let thunk_name = {
        let symbol_name = {
            // Call to `mono` is ok - `generics_of` have been checked above.
            let instance = ty::Instance::mono(tcx, def_id);
            tcx.symbol_name(instance).name
        };
        if needs_thunk {
            format!("__crubit_thunk_{}", &escape_non_identifier_chars(symbol_name))
        } else {
            symbol_name.to_string()
        }
    };

    let fully_qualified_fn_name = FullyQualifiedName::new(tcx, def_id);
    let unqualified_rust_fn_name =
        fully_qualified_fn_name.name.expect("Functions are assumed to always have a name");
    let attribute = crubit_attr::get(tcx, def_id).unwrap();
    let cpp_name = attribute.cpp_name;
    // The generated C++ function name.
    let main_api_fn_name = format_cc_ident(cpp_name.unwrap_or(unqualified_rust_fn_name).as_str())
        .context("Error formatting function name")?;

    let mut main_api_prereqs = CcPrerequisites::default();
    let main_api_ret_type = format_ret_ty_for_cc(db, &sig)?.into_tokens(&mut main_api_prereqs);

    struct Param<'tcx> {
        cc_name: TokenStream,
        cc_type: TokenStream,
        ty: Ty<'tcx>,
    }
    let params = {
        let names = tcx.fn_arg_names(def_id).iter();
        let cc_types = format_param_types_for_cc(db, &sig)?;
        names
            .enumerate()
            .zip(sig.inputs().iter())
            .zip(cc_types)
            .map(|(((i, name), &ty), cc_type)| {
                let cc_name = format_cc_ident(name.as_str())
                    .unwrap_or_else(|_err| format_cc_ident(&format!("__param_{i}")).unwrap());
                let cc_type = cc_type.into_tokens(&mut main_api_prereqs);
                Param { cc_name, cc_type, ty }
            })
            .collect_vec()
    };

    let self_ty: Option<Ty> = match tcx.impl_of_method(def_id) {
        Some(impl_id) => match tcx.impl_subject(impl_id).instantiate_identity() {
            ty::ImplSubject::Inherent(ty) => Some(ty),
            ty::ImplSubject::Trait(_) => panic!("Trait methods should be filtered by caller"),
        },
        None => None,
    };

    let method_kind = match tcx.hir_node_by_def_id(local_def_id) {
        Node::Item(_) => FunctionKind::Free,
        Node::ImplItem(_) => match tcx.fn_arg_names(def_id).first() {
            Some(arg_name) if arg_name.name == kw::SelfLower => {
                let self_ty = self_ty.expect("ImplItem => non-None `self_ty`");
                if params[0].ty == self_ty {
                    FunctionKind::MethodTakingSelfByValue
                } else {
                    match params[0].ty.kind() {
                        ty::TyKind::Ref(_, referent_ty, _) if *referent_ty == self_ty => {
                            FunctionKind::MethodTakingSelfByRef
                        }
                        _ => bail!("Unsupported `self` type"),
                    }
                }
            }
            _ => FunctionKind::StaticMethod,
        },
        other => panic!("Unexpected HIR node kind: {other:?}"),
    };
    let method_qualifiers = match method_kind {
        FunctionKind::Free | FunctionKind::StaticMethod => quote! {},
        FunctionKind::MethodTakingSelfByValue => quote! { && },
        FunctionKind::MethodTakingSelfByRef => match params[0].ty.kind() {
            ty::TyKind::Ref(region, _, mutability) => {
                let lifetime_annotation = format_region_as_cc_lifetime(region);
                let mutability = match mutability {
                    Mutability::Mut => quote! {},
                    Mutability::Not => quote! { const },
                };
                quote! { #mutability #lifetime_annotation }
            }
            _ => panic!("Expecting TyKind::Ref for MethodKind...Self...Ref"),
        },
    };

    let struct_name = match self_ty {
        Some(ty) => match ty.kind() {
            ty::TyKind::Adt(adt, substs) => {
                assert_eq!(0, substs.len(), "Callers should filter out generics");
                Some(FullyQualifiedName::new(tcx, adt.did()))
            }
            _ => panic!("Non-ADT `impl`s should be filtered by caller"),
        },
        None => None,
    };
    let needs_definition = unqualified_rust_fn_name.as_str() != thunk_name;
    let main_api_params = params
        .iter()
        .skip(if method_kind.has_self_param() { 1 } else { 0 })
        .map(|Param { cc_name, cc_type, .. }| quote! { #cc_type #cc_name })
        .collect_vec();
    let main_api = {
        let doc_comment = {
            let doc_comment = format_doc_comment(tcx, local_def_id);
            quote! { __NEWLINE__ #doc_comment }
        };

        let mut prereqs = main_api_prereqs.clone();
        prereqs.move_defs_to_fwd_decls();

        let static_ = if method_kind == FunctionKind::StaticMethod {
            quote! { static }
        } else {
            quote! {}
        };
        let extern_c = if !needs_definition {
            quote! { extern "C" }
        } else {
            quote! {}
        };

        let mut attributes = vec![];
        // Attribute: must_use
        if let Some(must_use_attr) = tcx.get_attr(def_id, rustc_span::symbol::sym::must_use) {
            match must_use_attr.value_str() {
                None => attributes.push(quote! {[[nodiscard]]}),
                Some(symbol) => {
                    let message = symbol.as_str();
                    attributes.push(quote! {[[nodiscard(#message)]]});
                }
            };
        }
        // Attribute: deprecated
        if let Some(cc_deprecated_tag) = format_deprecated_tag(tcx, def_id) {
            attributes.push(cc_deprecated_tag);
        }
        // Also check the impl block to which this function belongs (if there is one).
        // Note: parent_def_id can be Some(...) even if the function is not inside an
        // impl block.
        if let Some(parent_def_id) = tcx.opt_parent(def_id) {
            if let Some(cc_deprecated_tag) = format_deprecated_tag(tcx, parent_def_id) {
                attributes.push(cc_deprecated_tag);
            }
        }

        CcSnippet {
            prereqs,
            tokens: quote! {
                __NEWLINE__
                #doc_comment
                #extern_c #(#attributes)* #static_
                    #main_api_ret_type #main_api_fn_name (
                        #( #main_api_params ),*
                    ) #method_qualifiers;
                __NEWLINE__
            },
        }
    };
    let cc_details = if !needs_definition {
        CcSnippet::default()
    } else {
        let thunk_name = format_cc_ident(&thunk_name).context("Error formatting thunk name")?;
        let struct_name = match struct_name.as_ref() {
            None => quote! {},
            Some(fully_qualified_name) => {
                let name = fully_qualified_name.name.expect("Structs always have a name");
                let name = format_cc_ident(name.as_str())
                    .expect("Caller of format_fn should verify struct via format_adt_core");
                quote! { #name :: }
            }
        };

        let mut prereqs = main_api_prereqs;
        let thunk_decl =
            format_thunk_decl(db, def_id, &sig, &thunk_name)?.into_tokens(&mut prereqs);

        let mut thunk_args = params
            .iter()
            .enumerate()
            .map(|(i, Param { cc_name, ty, .. })| {
                if i == 0 && method_kind.has_self_param() {
                    if method_kind == FunctionKind::MethodTakingSelfByValue {
                        quote! { this }
                    } else {
                        quote! { *this }
                    }
                } else if is_c_abi_compatible_by_value(*ty) {
                    quote! { #cc_name }
                } else {
                    quote! { & #cc_name }
                }
            })
            .collect_vec();
        let impl_body: TokenStream;
        if is_c_abi_compatible_by_value(sig.output()) {
            impl_body = quote! {
                return __crubit_internal :: #thunk_name( #( #thunk_args ),* );
            };
        } else {
            if let Some(adt_def) = sig.output().ty_adt_def() {
                let core = db.format_adt_core(adt_def.did())?;
                db.format_move_ctor_and_assignment_operator(core).map_err(|_| {
                    anyhow!("Can't pass the return type by value without a move constructor")
                })?;
            }
            thunk_args.push(quote! { __ret_slot.Get() });
            impl_body = quote! {
                crubit::ReturnValueSlot<#main_api_ret_type> __ret_slot;
                __crubit_internal :: #thunk_name( #( #thunk_args ),* );
                return std::move(__ret_slot).AssumeInitAndTakeValue();
            };
            prereqs.includes.insert(CcInclude::utility()); // for `std::move`
            prereqs.includes.insert(db.support_header("internal/return_value_slot.h"));
        };
        CcSnippet {
            prereqs,
            tokens: quote! {
                __NEWLINE__
                #thunk_decl
                inline #main_api_ret_type #struct_name #main_api_fn_name (
                        #( #main_api_params ),* ) #method_qualifiers {
                    #impl_body
                }
                __NEWLINE__
            },
        }
    };

    let rs_details = if !needs_thunk {
        quote! {}
    } else {
        let fully_qualified_fn_name = match struct_name.as_ref() {
            None => fully_qualified_fn_name.format_for_rs(),
            Some(struct_name) => {
                let fn_name = make_rs_ident(unqualified_rust_fn_name.as_str());
                let struct_name = struct_name.format_for_rs();
                quote! { #struct_name :: #fn_name }
            }
        };
        format_thunk_impl(tcx, def_id, &sig, &thunk_name, fully_qualified_fn_name)?
    };
    Ok(ApiSnippets { main_api, cc_details, rs_details })
}

/// Represents bindings for the "core" part of an algebraic data type (an ADT -
/// a struct, an enum, or a union) in a way that supports later injecting the
/// other parts like so:
///
/// ```
/// quote! {
///     #keyword #alignment #name final {
///         #core
///         #decls_of_other_parts  // (e.g. struct fields, methods, etc.)
///     }
/// }
/// ```
///
/// `keyword`, `name` are stored separately, to support formatting them as a
/// forward declaration - e.g. `struct SomeStruct`.
#[derive(Clone)]
struct AdtCoreBindings<'tcx> {
    /// DefId of the ADT.
    def_id: DefId,

    /// C++ tag - e.g. `struct`, `class`, `enum`, or `union`.  This isn't always
    /// a direct mapping from Rust (e.g. a Rust `enum` might end up being
    /// represented as an opaque C++ `struct`).
    keyword: TokenStream,

    /// C++ translation of the ADT identifier - e.g. `SomeStruct`.
    ///
    /// A _short_ name is sufficient (i.e. there is no need to use a
    /// namespace-qualified name), for `CcSnippet`s that are emitted into
    /// the same namespace as the ADT.  (This seems to be all the snippets
    /// today.)
    cc_short_name: TokenStream,

    /// Rust spelling of the ADT type - e.g.
    /// `::some_crate::some_module::SomeStruct`.
    rs_fully_qualified_name: TokenStream,

    self_ty: Ty<'tcx>,
    alignment_in_bytes: u64,
    size_in_bytes: u64,
}

// AdtCoreBindings are a pure (and memoized...) function of the def_id.
impl<'tcx> PartialEq for AdtCoreBindings<'tcx> {
    fn eq(&self, other: &Self) -> bool {
        self.def_id == other.def_id
    }
}

impl<'tcx> Eq for AdtCoreBindings<'tcx> {}
impl<'tcx> Hash for AdtCoreBindings<'tcx> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.def_id.hash(state);
    }
}

impl<'tcx> AdtCoreBindings<'tcx> {
    fn needs_drop(&self, tcx: TyCtxt<'tcx>) -> bool {
        self.self_ty.needs_drop(tcx, tcx.param_env(self.def_id))
    }
}

/// Like `TyCtxt::is_directly_public`, but works not only with `LocalDefId`, but
/// also with `DefId`.
fn is_directly_public(tcx: TyCtxt, def_id: DefId) -> bool {
    match def_id.as_local() {
        None => {
            // This mimics the checks in `try_print_visible_def_path_recur` in
            // `compiler/rustc_middle/src/ty/print/pretty.rs`.
            let actual_parent = tcx.opt_parent(def_id);
            let visible_parent = tcx.visible_parent_map(()).get(&def_id).copied();
            actual_parent == visible_parent
        }
        Some(local_def_id) => tcx.effective_visibilities(()).is_directly_public(local_def_id),
    }
}

fn get_layout<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Result<Layout<'tcx>> {
    let param_env = match ty.ty_adt_def() {
        None => ty::ParamEnv::empty(),
        Some(adt_def) => tcx.param_env(adt_def.did()),
    };

    tcx.layout_of(param_env.and(ty)).map(|ty_and_layout| ty_and_layout.layout).map_err(
        |layout_err| {
            // Have to use `.map_err`, because `LayoutError` doesn't satisfy the
            // `anyhow::context::ext::StdError` trait bound.
            anyhow!("Error computing the layout: {layout_err}")
        },
    )
}

/// Formats the core of an algebraic data type (an ADT - a struct, an enum, or a
/// union) represented by `def_id`.
///
/// The "core" means things that are necessary for a succesful binding (e.g.
/// inability to generate a correct C++ destructor means that the ADT cannot
/// have any bindings).  "core" excludes things that are A) infallible (e.g.
/// struct or union fields which can always be translated into private, opaque
/// blobs of bytes) or B) optional (e.g. a problematic instance method
/// can just be ignored, unlike a problematic destructor).  The split between
/// fallible "core" and non-fallible "rest" is motivated by the need to avoid
/// cycles / infinite recursion (e.g. when processing fields that refer back to
/// the struct type, possible with an indirection of a pointer).
///
/// `format_adt_core` is used both to 1) format bindings for the core of an ADT,
/// and 2) check if formatting would have succeeded (e.g. when called from
/// `format_ty`).  The 2nd case is needed for ADTs defined in any crate - this
/// is why the `def_id` parameter is a DefId rather than LocalDefId.
fn format_adt_core<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    def_id: DefId,
) -> Result<Rc<AdtCoreBindings<'tcx>>> {
    let tcx = db.tcx();
    let self_ty = tcx.type_of(def_id).instantiate_identity();
    assert!(self_ty.is_adt());
    assert!(is_directly_public(tcx, def_id), "Caller should verify");

    let item_name = tcx.item_name(def_id);
    let rs_fully_qualified_name = format_ty_for_rs(tcx, self_ty)?;
    let cc_short_name =
        format_cc_ident(item_name.as_str()).context("Error formatting item name")?;

    // The check below ensures that `format_trait_thunks` will succeed for the
    // `Drop`, `Default`, and/or `Clone` trait. Ideally we would directly check
    // if `format_trait_thunks` or `format_ty_for_cc(..., self_ty, ...)`
    // succeeds, but this would lead to infinite recursion, so we only replicate
    // `format_ty_for_cc` / `TyKind::Adt` checks that are outside of
    // `format_adt_core`.
    FullyQualifiedName::new(tcx, def_id).format_for_cc().with_context(|| {
        format!("Error formatting the fully-qualified C++ name of `{item_name}")
    })?;

    let adt_def = self_ty.ty_adt_def().expect("`def_id` needs to identify an ADT");
    let keyword = match adt_def.adt_kind() {
        ty::AdtKind::Struct | ty::AdtKind::Enum => quote! { struct },
        ty::AdtKind::Union => quote! { union },
    };

    let layout = get_layout(tcx, self_ty)
        .with_context(|| format!("Error computing the layout of #{item_name}"))?;
    ensure!(layout.abi().is_sized(), "Bindings for dynamically sized types are not supported.");
    let alignment_in_bytes = {
        // Only the ABI-mandated alignment is considered (i.e. `AbiAndPrefAlign::pref`
        // is ignored), because 1) Rust's `std::mem::align_of` returns the
        // ABI-mandated alignment and 2) the generated C++'s `alignas(...)`
        // should specify the minimal/mandatory alignment.
        layout.align().abi.bytes()
    };
    let size_in_bytes = layout.size().bytes();
    ensure!(size_in_bytes != 0, "Zero-sized types (ZSTs) are not supported (b/258259459)");

    Ok(Rc::new(AdtCoreBindings {
        def_id,
        keyword,
        cc_short_name,
        rs_fully_qualified_name,
        self_ty,
        alignment_in_bytes,
        size_in_bytes,
    }))
}

fn repr_attrs(db: &dyn BindingsGenerator<'_>, def_id: DefId) -> Rc<[rustc_attr::ReprAttr]> {
    let tcx = db.tcx();
    let attrs: Vec<_> = tcx
        .get_attrs(def_id, sym::repr)
        .flat_map(|attr| rustc_attr::parse_repr_attr(tcx.sess(), attr))
        .collect();
    attrs.into()
}

fn format_fields<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    core: &AdtCoreBindings<'tcx>,
) -> ApiSnippets {
    let tcx = db.tcx();

    // TODO(b/259749095): Support non-empty set of generic parameters.
    let substs_ref = ty::List::empty();

    struct FieldTypeInfo {
        size: u64,
        cc_type: CcSnippet,
    }
    struct Field {
        type_info: Result<FieldTypeInfo>,
        cc_name: TokenStream,
        rs_name: TokenStream,
        is_public: bool,
        index: usize,
        offset: u64,
        offset_of_next_field: u64,
        doc_comment: TokenStream,
        attributes: Vec<TokenStream>,
    }
    impl Field {
        fn size(&self) -> u64 {
            match self.type_info {
                Err(_) => self.offset_of_next_field - self.offset,
                Ok(FieldTypeInfo { size, .. }) => size,
            }
        }
    }

    let layout = get_layout(tcx, core.self_ty)
        .expect("Layout should be already verified by `format_adt_core`");
    let adt_def = core.self_ty.ty_adt_def().expect("`core.def_id` needs to identify an ADT");
    let fields: Vec<Field> = if core.self_ty.is_enum() {
        vec![Field {
            type_info: Err(anyhow!("No support for bindings of individual `enum` fields")),
            cc_name: quote! { __opaque_blob_of_bytes },
            rs_name: quote! { __opaque_blob_of_bytes },
            is_public: false,
            index: 0,
            offset: 0,
            offset_of_next_field: core.size_in_bytes,
            doc_comment: quote! {},
            attributes: vec![],
        }]
    } else {
        let mut fields = core
            .self_ty
            .ty_adt_def()
            .expect("`core.def_id` needs to identify an ADT")
            .all_fields()
            .sorted_by_key(|f| tcx.def_span(f.did))
            .enumerate()
            .map(|(index, field_def)| {
                let field_ty = field_def.ty(tcx, substs_ref);
                let size = get_layout(tcx, field_ty).map(|layout| layout.size().bytes());
                let type_info = size.and_then(|size| {
                    Ok(FieldTypeInfo {
                        size,
                        cc_type: db.format_ty_for_cc(field_ty, TypeLocation::Other)?,
                    })
                });
                let name = field_def.ident(tcx);
                let cc_name = format_cc_ident(name.as_str())
                    .unwrap_or_else(|_err| format_ident!("__field{index}").into_token_stream());
                let rs_name = {
                    let name_starts_with_digit = name
                        .as_str()
                        .chars()
                        .next()
                        .expect("Empty names are unexpected (here and in general)")
                        .is_ascii_digit();
                    if name_starts_with_digit {
                        let index = Literal::usize_unsuffixed(index);
                        quote! { #index }
                    } else {
                        let name = make_rs_ident(name.as_str());
                        quote! { #name }
                    }
                };

                // `offset` and `offset_of_next_field` will be fixed by FieldsShape::Arbitrary
                // branch below.
                let offset = 0;
                let offset_of_next_field = 0;

                // Populate attributes.
                let mut attributes = vec![];
                if let Some(cc_deprecated_tag) = format_deprecated_tag(tcx, field_def.did) {
                    attributes.push(cc_deprecated_tag);
                }

                Field {
                    type_info,
                    cc_name,
                    rs_name,
                    is_public: field_def.vis == ty::Visibility::Public,
                    index,
                    offset,
                    offset_of_next_field,
                    doc_comment: format_doc_comment(tcx, field_def.did.expect_local()),
                    attributes,
                }
            })
            .collect_vec();

        // Determine the memory layout
        match layout.fields() {
            FieldsShape::Arbitrary { offsets, .. } => {
                for (index, offset) in offsets.iter().enumerate() {
                    // Documentation of `FieldsShape::Arbitrary says that the offsets are
                    // "ordered to match the source definition order".
                    // We can coorelate them with elements
                    // of the `fields` vector because we've explicitly `sorted_by_key` using
                    // `def_span`.
                    fields[index].offset = offset.bytes();
                }
                // Sort by offset first; ZSTs in the same offset are sorted by source order.
                // Use `field_size` to ensure ZSTs at the same offset as
                // non-ZSTs sort first to avoid weird offset issues later on.
                fields.sort_by_key(|field| {
                    let field_size = field.type_info.as_ref().map(|info| info.size).unwrap_or(0);
                    (field.offset, field_size, field.index)
                });
            }
            FieldsShape::Union(num_fields) => {
                // Compute the offset of each field
                for index in 0..num_fields.get() {
                    fields[index].offset = layout.fields().offset(index).bytes();
                }
            }
            unexpected => panic!("Unexpected FieldsShape: {unexpected:?}"),
        }

        let next_offsets = fields
            .iter()
            .map(|Field { offset, .. }| *offset)
            .skip(1)
            .chain(once(core.size_in_bytes))
            .collect_vec();
        for (field, next_offset) in fields.iter_mut().zip(next_offsets) {
            field.offset_of_next_field = next_offset;
        }
        fields
    };

    let cc_details = if fields.is_empty() {
        CcSnippet::default()
    } else {
        let adt_cc_name = &core.cc_short_name;
        let cc_assertions: TokenStream = fields
            .iter()
            // TODO(b/298660437): Add support for ZST fields.
            .filter(|field| field.size() != 0)
            .map(|Field { cc_name, offset, .. }| {
                let offset = Literal::u64_unsuffixed(*offset);
                quote! { static_assert(#offset == offsetof(#adt_cc_name, #cc_name)); }
            })
            .collect();
        CcSnippet::with_include(
            quote! {
                inline void #adt_cc_name::__crubit_field_offset_assertions() {
                    #cc_assertions
                }
            },
            CcInclude::cstddef(),
        )
    };
    let rs_details: TokenStream = {
        let adt_rs_name = &core.rs_fully_qualified_name;
        fields
            .iter()
            // TODO(b/298660437): Even though we don't generate bindings for ZST fields, we'd still
            // like to make sure we computed the offset of ZST fields correctly on the Rust side,
            // so we still emit offset assertions for ZST fields here.
            // TODO(b/298660437): Remove the comment above when ZST fields are supported.
            .filter(|field| field.is_public)
            .map(|Field { rs_name, offset, .. }| {
                let expected_offset = Literal::u64_unsuffixed(*offset);
                let actual_offset = quote! { ::core::mem::offset_of!(#adt_rs_name, #rs_name) };
                quote! { const _: () = assert!(#actual_offset == #expected_offset); }
            })
            .collect()
    };
    let main_api = {
        let assertions_method_decl = if fields.is_empty() {
            quote! {}
        } else {
            // We put the assertions in a method so that they can read private member
            // variables.
            quote! { private: static void __crubit_field_offset_assertions(); }
        };

        // If all fields are known, and the type is repr(C), then we don't need padding
        // fields, and can instead use the natural padding from alignment.
        //
        // Note: it does need to be repr(C) to be guaranteed, since the compiler might
        // reasonably place a field later than it has to for layout
        // randomization purposes. For example, in `#[repr(align(4))] struct
        // Foo(i8);` there are four different places the `i8` could be.
        // If it was placed in the second byte, for any reason, then we would need
        // explicit padding bytes.
        let repr_attrs = db.repr_attrs(core.def_id);
        let always_omit_padding = repr_attrs.contains(&rustc_attr::ReprC)
            && fields.iter().all(|field| field.type_info.is_ok());

        let mut prereqs = CcPrerequisites::default();
        let fields: TokenStream = fields
            .into_iter()
            .map(|field| {
                let cc_name = &field.cc_name;
                match field.type_info {
                    Err(ref err) => {
                        let size = field.size();
                        let msg =
                            format!("Field type has been replaced with a blob of bytes: {err:#}");

                        // Empty arrays are ill-formed, but also unnecessary for padding.
                        if size > 0 {
                            let size = Literal::u64_unsuffixed(size);
                            quote! {
                                private: __NEWLINE__
                                    __COMMENT__ #msg
                                    unsigned char #cc_name[#size];
                            }
                        } else {
                            // TODO(b/258259459): Generate bindings for ZST fields.
                            let msg = format!(
                                "Skipped bindings for field `{cc_name}`: \
                               ZST fields are not supported (b/258259459)"
                            );
                            quote! {__NEWLINE__ __COMMENT__ #msg}
                        }
                    }
                    Ok(FieldTypeInfo { cc_type, size }) => {
                        // Only structs require no overlaps.
                        let padding = match adt_def.adt_kind() {
                            ty::AdtKind::Struct => {
                                assert!((field.offset + size) <= field.offset_of_next_field);
                                field.offset_of_next_field - field.offset - size
                            }
                            ty::AdtKind::Union => field.offset,
                            ty::AdtKind::Enum => todo!(),
                        };

                        // Omit explicit padding if:
                        //   1. The type is repr(C) and has known types for all fields, so we can
                        //      reuse the natural repr(C) padding.
                        //   2. There is no padding
                        // TODO(jeanpierreda): also omit padding for the final field?
                        let padding = if always_omit_padding || padding == 0 {
                            quote! {}
                        } else {
                            let padding = Literal::u64_unsuffixed(padding);
                            let ident = format_ident!("__padding{}", field.index);
                            quote! { private: unsigned char #ident[#padding]; }
                        };
                        let visibility = if field.is_public {
                            quote! { public: }
                        } else {
                            quote! { private: }
                        };
                        let cc_type = cc_type.into_tokens(&mut prereqs);
                        let doc_comment = field.doc_comment;
                        let attributes = field.attributes;

                        match adt_def.adt_kind() {
                            ty::AdtKind::Struct => quote! {
                                #visibility __NEWLINE__
                                    // The anonymous union gives more control over when exactly
                                    // the field constructors and destructors run.  See also
                                    // b/288138612.
                                    union {  __NEWLINE__
                                        #doc_comment
                                        #(#attributes)*
                                        #cc_type #cc_name;
                                    };
                                #padding
                            },
                            ty::AdtKind::Union => {
                                if repr_attrs.contains(&rustc_attr::ReprC) {
                                    quote! {
                                        __NEWLINE__
                                        #doc_comment
                                        #cc_type #cc_name;
                                    }
                                } else {
                                     let internal_padding = if field.offset == 0 {
                                         quote! {}
                                        } else {
                                         let internal_padding_size = Literal::u64_unsuffixed(field.offset);
                                         quote! {char __crubit_internal_padding[#internal_padding_size]}
                                    };
                                    quote! {
                                        __NEWLINE__
                                        #doc_comment
                                        struct {
                                            #internal_padding
                                            #cc_type value;
                                        } #cc_name;
                                    }
                                }
                            }
                            ty::AdtKind::Enum => todo!(),
                        }
                    }
                }
            })
            .collect();

        CcSnippet {
            prereqs,
            tokens: quote! {
                #fields
                #assertions_method_decl
            },
        }
    };

    ApiSnippets { main_api, cc_details, rs_details }
}

fn does_type_implement_trait<'tcx>(tcx: TyCtxt<'tcx>, self_ty: Ty<'tcx>, trait_id: DefId) -> bool {
    assert!(tcx.is_trait(trait_id));

    let generics = tcx.generics_of(trait_id);
    assert!(generics.has_self);
    assert_eq!(
        generics.count(),
        1, // Only `Self`
        "Generic traits are not supported yet (b/286941486)",
    );
    let substs = [self_ty];

    tcx.infer_ctxt()
        .build()
        .type_implements_trait(trait_id, substs, tcx.param_env(trait_id))
        .must_apply_modulo_regions()
}

struct TraitThunks {
    method_name_to_cc_thunk_name: HashMap<Symbol, TokenStream>,
    cc_thunk_decls: CcSnippet,
    rs_thunk_impls: TokenStream,
}

fn format_trait_thunks<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    trait_id: DefId,
    adt: &AdtCoreBindings<'tcx>,
) -> Result<TraitThunks> {
    let tcx = db.tcx();
    assert!(tcx.is_trait(trait_id));

    let self_ty = adt.self_ty;
    let is_drop_trait = Some(trait_id) == tcx.lang_items().drop_trait();
    if is_drop_trait {
        // To support "drop glue" we don't require that `self_ty` directly implements
        // the `Drop` trait.  Instead we require the caller to check
        // `needs_drop`.
        assert!(self_ty.needs_drop(tcx, tcx.param_env(adt.def_id)));
    } else if !does_type_implement_trait(tcx, self_ty, trait_id) {
        let trait_name = tcx.item_name(trait_id);
        bail!("`{self_ty}` doesn't implement the `{trait_name}` trait");
    }

    let mut method_name_to_cc_thunk_name = HashMap::new();
    let mut cc_thunk_decls = CcSnippet::default();
    let mut rs_thunk_impls = quote! {};
    let methods = tcx
        .associated_items(trait_id)
        .in_definition_order()
        .filter(|item| item.kind == ty::AssocKind::Fn);
    for method in methods {
        let substs = {
            let generics = tcx.generics_of(method.def_id);
            if generics.own_params.iter().any(|p| p.kind.is_ty_or_const()) {
                // Note that lifetime-generic methods are ok:
                // * they are handled by `format_thunk_decl` and `format_thunk_impl`
                // * the lifetimes are erased by `ty::Instance::mono` and *seem* to be erased by
                //   `ty::Instance::new`
                panic!(
                    "So far callers of `format_trait_thunks` didn't need traits with \
                        methods that are type-generic or const-generic"
                );
            }
            assert!(generics.has_self);
            tcx.mk_args_trait(self_ty, std::iter::empty())
        };

        let thunk_name = {
            let instance = ty::Instance::new(method.def_id, substs);
            let symbol = tcx.symbol_name(instance);
            format!("__crubit_thunk_{}", &escape_non_identifier_chars(symbol.name))
        };
        method_name_to_cc_thunk_name.insert(method.name, format_cc_ident(&thunk_name)?);

        let sig = tcx.fn_sig(method.def_id).instantiate(tcx, substs);
        let sig = liberate_and_deanonymize_late_bound_regions(tcx, sig, method.def_id);

        cc_thunk_decls.add_assign({
            let thunk_name = format_cc_ident(&thunk_name)?;
            format_thunk_decl(db, method.def_id, &sig, &thunk_name)?
        });

        rs_thunk_impls.extend({
            let struct_name = &adt.rs_fully_qualified_name;
            if is_drop_trait {
                // Manually formatting (instead of depending on `format_thunk_impl`)
                // to avoid https://doc.rust-lang.org/error_codes/E0040.html
                let thunk_name = make_rs_ident(&thunk_name);
                quote! {
                    #[no_mangle]
                    extern "C" fn #thunk_name(
                        __self: &mut ::core::mem::MaybeUninit<#struct_name>
                    ) {
                        unsafe { __self.assume_init_drop() };
                    }
                }
            } else {
                let fully_qualified_fn_name = {
                    let fully_qualified_trait_name =
                        FullyQualifiedName::new(tcx, trait_id).format_for_rs();
                    let method_name = make_rs_ident(method.name.as_str());
                    quote! { <#struct_name as #fully_qualified_trait_name>::#method_name }
                };
                format_thunk_impl(tcx, method.def_id, &sig, &thunk_name, fully_qualified_fn_name)?
            }
        });
    }

    Ok(TraitThunks { method_name_to_cc_thunk_name, cc_thunk_decls, rs_thunk_impls })
}

/// Formats a default constructor for an ADT if possible (i.e. if the `Default`
/// trait is implemented for the ADT).  Returns an error otherwise (e.g. if
/// there is no `Default` impl, then the default constructor will be
/// `=delete`d in the returned snippet).
fn format_default_ctor<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    core: Rc<AdtCoreBindings<'tcx>>,
) -> Result<ApiSnippets, ApiSnippets> {
    fn fallible_format_default_ctor<'tcx>(
        db: &dyn BindingsGenerator<'tcx>,
        core: Rc<AdtCoreBindings<'tcx>>,
    ) -> Result<ApiSnippets> {
        let tcx = db.tcx();
        let trait_id = tcx
            .get_diagnostic_item(sym::Default)
            .ok_or(anyhow!("Couldn't find `core::default::Default`"))?;
        let TraitThunks {
            method_name_to_cc_thunk_name,
            cc_thunk_decls,
            rs_thunk_impls: rs_details,
        } = format_trait_thunks(db, trait_id, &core)?;

        let cc_struct_name = &core.cc_short_name;
        let main_api = CcSnippet::new(quote! {
            __NEWLINE__ __COMMENT__ "Default::default"
            #cc_struct_name(); __NEWLINE__ __NEWLINE__
        });
        let cc_details = {
            let thunk_name = method_name_to_cc_thunk_name
                .into_values()
                .exactly_one()
                .expect("Expecting a single `default` method");

            let mut prereqs = CcPrerequisites::default();
            let cc_thunk_decls = cc_thunk_decls.into_tokens(&mut prereqs);

            let tokens = quote! {
                #cc_thunk_decls
                inline #cc_struct_name::#cc_struct_name() {
                    __crubit_internal::#thunk_name(this);
                }
            };
            CcSnippet { tokens, prereqs }
        };
        Ok(ApiSnippets { main_api, cc_details, rs_details })
    }
    fallible_format_default_ctor(db, core.clone()).map_err(|err| {
        let msg = format!("{err:#}");
        let adt_cc_name = &core.cc_short_name;
        ApiSnippets {
            main_api: CcSnippet::new(quote! {
                __NEWLINE__ __COMMENT__ #msg
                #adt_cc_name() = delete; __NEWLINE__
            }),
            ..Default::default()
        }
    })
}

/// Formats the copy constructor and the copy-assignment operator for an ADT if
/// possible (i.e. if the `Clone` trait is implemented for the ADT).  Returns an
/// error otherwise (e.g. if there is no `Clone` impl, then the copy constructor
/// and assignment operator will be `=delete`d in the returned snippet).
fn format_copy_ctor_and_assignment_operator<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    core: Rc<AdtCoreBindings<'tcx>>,
) -> Result<ApiSnippets, ApiSnippets> {
    fn fallible_format_copy_ctor_and_assignment_operator<'tcx>(
        db: &dyn BindingsGenerator<'tcx>,
        core: Rc<AdtCoreBindings<'tcx>>,
    ) -> Result<ApiSnippets> {
        let tcx = db.tcx();
        let cc_struct_name = &core.cc_short_name;

        let is_copy = {
            // TODO(b/259749095): Once generic ADTs are supported, `is_copy_modulo_regions`
            // might need to be replaced with a more thorough check - see
            // b/258249993#comment4.
            core.self_ty.is_copy_modulo_regions(tcx, tcx.param_env(core.def_id))
        };
        if is_copy {
            let msg = "Rust types that are `Copy` get trivial, `default` C++ copy constructor \
                       and assignment operator.";
            let main_api = CcSnippet::new(quote! {
                __NEWLINE__ __COMMENT__ #msg
                #cc_struct_name(const #cc_struct_name&) = default;  __NEWLINE__
                #cc_struct_name& operator=(const #cc_struct_name&) = default;
            });
            let cc_details = CcSnippet::with_include(
                quote! {
                    static_assert(std::is_trivially_copy_constructible_v<#cc_struct_name>);
                    static_assert(std::is_trivially_copy_assignable_v<#cc_struct_name>);
                },
                CcInclude::type_traits(),
            );

            return Ok(ApiSnippets { main_api, cc_details, rs_details: quote! {} });
        }

        let trait_id = tcx
            .lang_items()
            .clone_trait()
            .ok_or_else(|| anyhow!("Can't find the `Clone` trait"))?;
        let TraitThunks {
            method_name_to_cc_thunk_name,
            cc_thunk_decls,
            rs_thunk_impls: rs_details,
        } = format_trait_thunks(db, trait_id, &core)?;
        let main_api = CcSnippet::new(quote! {
            __NEWLINE__ __COMMENT__ "Clone::clone"
            #cc_struct_name(const #cc_struct_name&); __NEWLINE__
            __NEWLINE__ __COMMENT__ "Clone::clone_from"
            #cc_struct_name& operator=(const #cc_struct_name&); __NEWLINE__ __NEWLINE__
        });
        let cc_details = {
            // `unwrap` calls are okay because `Clone` trait always has these methods.
            let clone_thunk_name = method_name_to_cc_thunk_name.get(&sym::clone).unwrap();
            let clone_from_thunk_name = method_name_to_cc_thunk_name.get(&sym::clone_from).unwrap();

            let mut prereqs = CcPrerequisites::default();
            let cc_thunk_decls = cc_thunk_decls.into_tokens(&mut prereqs);

            let tokens = quote! {
                #cc_thunk_decls
                inline #cc_struct_name::#cc_struct_name(const #cc_struct_name& other) {
                    __crubit_internal::#clone_thunk_name(other, this);
                }
                inline #cc_struct_name& #cc_struct_name::operator=(const #cc_struct_name& other) {
                    if (this != &other) {
                        __crubit_internal::#clone_from_thunk_name(*this, other);
                    }
                    return *this;
                }
            };
            CcSnippet { tokens, prereqs }
        };
        Ok(ApiSnippets { main_api, cc_details, rs_details })
    }
    fallible_format_copy_ctor_and_assignment_operator(db, core.clone()).map_err(|err| {
        let msg = format!("{err:#}");
        let adt_cc_name = &core.cc_short_name;
        ApiSnippets {
            main_api: CcSnippet::new(quote! {
                __NEWLINE__ __COMMENT__ #msg
                #adt_cc_name(const #adt_cc_name&) = delete;  __NEWLINE__
                #adt_cc_name& operator=(const #adt_cc_name&) = delete;
            }),
            ..Default::default()
        }
    })
}

/// Formats the move constructor and the move-assignment operator for an ADT if
/// possible (it depends on various factors like `needs_drop`, `is_unpin` and
/// implementations of `Default` and/or `Clone` traits).  Returns an error
/// otherwise (the error's `ApiSnippets` contain a `=delete`d declaration).
fn format_move_ctor_and_assignment_operator<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    core: Rc<AdtCoreBindings<'tcx>>,
) -> Result<ApiSnippets, ApiSnippets> {
    fn fallible_format_move_ctor_and_assignment_operator<'tcx>(
        db: &dyn BindingsGenerator<'tcx>,
        core: Rc<AdtCoreBindings<'tcx>>,
    ) -> Result<ApiSnippets> {
        let tcx = db.tcx();
        let adt_cc_name = &core.cc_short_name;
        if core.needs_drop(tcx) {
            let has_default_ctor = db.format_default_ctor(core.clone()).is_ok();
            let is_unpin = core.self_ty.is_unpin(tcx, tcx.param_env(core.def_id));
            if has_default_ctor && is_unpin {
                let main_api = CcSnippet::new(quote! {
                    #adt_cc_name(#adt_cc_name&&); __NEWLINE__
                    #adt_cc_name& operator=(#adt_cc_name&&); __NEWLINE__
                });
                let mut prereqs = CcPrerequisites::default();
                prereqs.includes.insert(db.support_header("internal/memswap.h"));
                prereqs.includes.insert(CcInclude::utility()); // for `std::move`
                let tokens = quote! {
                    inline #adt_cc_name::#adt_cc_name(#adt_cc_name&& other)
                            : #adt_cc_name() {
                        *this = std::move(other);
                    }
                    inline #adt_cc_name& #adt_cc_name::operator=(#adt_cc_name&& other) {
                        crubit::MemSwap(*this, other);
                        return *this;
                    }
                };
                let cc_details = CcSnippet { tokens, prereqs };
                Ok(ApiSnippets { main_api, cc_details, ..Default::default() })
            } else if db.format_copy_ctor_and_assignment_operator(core).is_ok() {
                // The class will have a custom copy constructor and copy assignment operator
                // and *no* move constructor nor move assignment operator. This
                // way, when a move is requested, a copy is performed instead
                // (this is okay, this is what happens if a copyable pre-C++11
                // class is compiled in C++11 mode and moved).
                //
                // We can't use the `=default` move constructor, because it is elementwise and
                // semantically incorrect.  We can't `=delete` the move constructor because it
                // would make `SomeStruct(MakeSomeStruct())` select the deleted move constructor
                // and fail to compile.
                Ok(ApiSnippets::default())
            } else {
                bail!(
                    "C++ moves are deleted \
                       because there's no non-destructive implementation available."
                );
            }
        } else {
            let main_api = CcSnippet::new(quote! {
                // The generated bindings have to follow Rust move semantics:
                // * All Rust types are memcpy-movable (e.g. <internal link>/constructors.html says
                //   that "Every type must be ready for it to be blindly memcopied to somewhere
                //   else in memory")
                // * The only valid operation on a moved-from non-`Copy` Rust struct is to assign to
                //   it.
                //
                // The generated C++ bindings below match the required semantics because they:
                // * Generate trivial` C++ move constructor and move assignment operator. Per
                //   <internal link>/cpp/language/move_constructor#Trivial_move_constructor: "A trivial
                //   move constructor is a constructor that performs the same action as the trivial
                //   copy constructor, that is, makes a copy of the object representation as if by
                //   std::memmove."
                // * Generate trivial C++ destructor.
                //
                // In particular, note that the following C++ code and Rust code are exactly
                // equivalent (except that in Rust, reuse of `y` is forbidden at compile time,
                // whereas in C++, it's only prohibited by convention):
                // * C++, assumming trivial move constructor and trivial destructor:
                //   `auto x = std::move(y);`
                // * Rust, assumming non-`Copy`, no custom `Drop` or drop glue:
                //   `let x = y;`
                //
                // TODO(b/258251148): If the ADT provides a custom `Drop` impls or requires drop
                // glue, then extra care should be taken to ensure the C++ destructor can handle
                // the moved-from object in a way that meets Rust move semantics.  For example, the
                // generated C++ move constructor might need to assign `Default::default()` to the
                // moved-from object.
                #adt_cc_name(#adt_cc_name&&) = default; __NEWLINE__
                #adt_cc_name& operator=(#adt_cc_name&&) = default; __NEWLINE__
                __NEWLINE__
            });
            let cc_details = CcSnippet::with_include(
                quote! {
                    static_assert(std::is_trivially_move_constructible_v<#adt_cc_name>);
                    static_assert(std::is_trivially_move_assignable_v<#adt_cc_name>);
                },
                CcInclude::type_traits(),
            );
            Ok(ApiSnippets { main_api, cc_details, ..Default::default() })
        }
    }
    fallible_format_move_ctor_and_assignment_operator(db, core.clone()).map_err(|err| {
        let msg = format!("{err:#}");
        let adt_cc_name = &core.cc_short_name;
        ApiSnippets {
            main_api: CcSnippet::new(quote! {
                __NEWLINE__ __COMMENT__ #msg
                #adt_cc_name(#adt_cc_name&&) = delete;  __NEWLINE__
                #adt_cc_name& operator=(#adt_cc_name&&) = delete;
            }),
            ..Default::default()
        }
    })
}

/// Formats an algebraic data type (an ADT - a struct, an enum, or a union)
/// represented by `core`.  This function is infallible - after
/// `format_adt_core` returns success we have committed to emitting C++ bindings
/// for the ADT.
fn format_adt<'tcx>(
    db: &dyn BindingsGenerator<'tcx>,
    core: Rc<AdtCoreBindings<'tcx>>,
) -> ApiSnippets {
    let tcx = db.tcx();
    let adt_cc_name = &core.cc_short_name;

    // `format_adt` should only be called for local ADTs.
    let local_def_id = core.def_id.expect_local();

    let default_ctor_snippets = db.format_default_ctor(core.clone()).unwrap_or_else(|err| err);

    let destructor_snippets = if core.needs_drop(tcx) {
        let drop_trait_id =
            tcx.lang_items().drop_trait().expect("`Drop` trait should be present if `needs_drop");
        let TraitThunks {
            method_name_to_cc_thunk_name,
            cc_thunk_decls,
            rs_thunk_impls: rs_details,
        } = format_trait_thunks(db, drop_trait_id, &core)
            .expect("`format_adt_core` should have already validated `Drop` support");
        let drop_thunk_name = method_name_to_cc_thunk_name
            .into_values()
            .exactly_one()
            .expect("Expecting a single `drop` method");
        let main_api = CcSnippet::new(quote! {
            __NEWLINE__ __COMMENT__ "Drop::drop"
            ~#adt_cc_name(); __NEWLINE__
            __NEWLINE__
        });
        let cc_details = {
            let mut prereqs = CcPrerequisites::default();
            let cc_thunk_decls = cc_thunk_decls.into_tokens(&mut prereqs);
            let tokens = quote! {
                #cc_thunk_decls
                inline #adt_cc_name::~#adt_cc_name() {
                    __crubit_internal::#drop_thunk_name(*this);
                }
            };
            CcSnippet { tokens, prereqs }
        };
        ApiSnippets { main_api, cc_details, rs_details }
    } else {
        let main_api = CcSnippet::new(quote! {
            __NEWLINE__ __COMMENT__ "No custom `Drop` impl and no custom \"drop glue\" required"
            ~#adt_cc_name() = default; __NEWLINE__
        });
        let cc_details = CcSnippet::with_include(
            quote! { static_assert(std::is_trivially_destructible_v<#adt_cc_name>); },
            CcInclude::type_traits(),
        );
        ApiSnippets { main_api, cc_details, ..Default::default() }
    };

    let copy_ctor_and_assignment_snippets =
        db.format_copy_ctor_and_assignment_operator(core.clone()).unwrap_or_else(|err| err);

    let move_ctor_and_assignment_snippets =
        db.format_move_ctor_and_assignment_operator(core.clone()).unwrap_or_else(|err| err);

    let impl_items_snippets = tcx
        .inherent_impls(core.def_id)
        .into_iter()
        .flatten()
        .map(|impl_id| tcx.hir().expect_item(impl_id.expect_local()))
        .flat_map(|item| match &item.kind {
            ItemKind::Impl(impl_) => impl_.items,
            other => panic!("Unexpected `ItemKind` from `inherent_impls`: {other:?}"),
        })
        .sorted_by_key(|impl_item_ref| {
            let def_id = impl_item_ref.id.owner_id.def_id;
            tcx.def_span(def_id)
        })
        .filter_map(|impl_item_ref| {
            let def_id = impl_item_ref.id.owner_id.def_id;
            if !tcx.effective_visibilities(()).is_directly_public(def_id) {
                return None;
            }
            let result = match impl_item_ref.kind {
                AssocItemKind::Fn { .. } => db.format_fn(def_id).map(Some),
                other => Err(anyhow!("Unsupported `impl` item kind: {other:?}")),
            };
            result.unwrap_or_else(|err| Some(format_unsupported_def(db, def_id, err)))
        })
        .collect();

    let ApiSnippets {
        main_api: public_functions_main_api,
        cc_details: public_functions_cc_details,
        rs_details: public_functions_rs_details,
    } = [
        default_ctor_snippets,
        destructor_snippets,
        move_ctor_and_assignment_snippets,
        copy_ctor_and_assignment_snippets,
        impl_items_snippets,
    ]
    .into_iter()
    .collect();

    let ApiSnippets {
        main_api: fields_main_api,
        cc_details: fields_cc_details,
        rs_details: fields_rs_details,
    } = format_fields(db, &core);

    let alignment = Literal::u64_unsuffixed(core.alignment_in_bytes);
    let size = Literal::u64_unsuffixed(core.size_in_bytes);
    let main_api = {
        let rs_type = core.rs_fully_qualified_name.to_string();
        let mut attributes = vec![
            quote! {CRUBIT_INTERNAL_RUST_TYPE(#rs_type)},
            quote! {alignas(#alignment)},
            quote! {[[clang::trivial_abi]]},
        ];
        if db
            .repr_attrs(core.def_id)
            .iter()
            .any(|repr| matches!(repr, rustc_attr::ReprPacked { .. }))
        {
            attributes.push(quote! { __attribute__((packed)) })
        }

        // Attribute: must_use
        if let Some(must_use_attr) = tcx.get_attr(core.def_id, rustc_span::symbol::sym::must_use) {
            match must_use_attr.value_str() {
                None => attributes.push(quote! {[[nodiscard]]}),
                Some(symbol) => {
                    let message = symbol.as_str();
                    attributes.push(quote! {[[nodiscard(#message)]]});
                }
            }
        }

        // Attribute: deprecated
        if let Some(cc_deprecated_tag) = format_deprecated_tag(tcx, core.def_id) {
            attributes.push(cc_deprecated_tag);
        }

        let doc_comment = format_doc_comment(tcx, core.def_id.expect_local());
        let keyword = &core.keyword;

        let mut prereqs = CcPrerequisites::default();
        prereqs.includes.insert(db.support_header("internal/attribute_macros.h"));
        let public_functions_main_api = public_functions_main_api.into_tokens(&mut prereqs);
        let fields_main_api = fields_main_api.into_tokens(&mut prereqs);
        prereqs.fwd_decls.remove(&local_def_id);

        CcSnippet {
            prereqs,
            tokens: quote! {
                __NEWLINE__ #doc_comment
                #keyword #(#attributes)* #adt_cc_name final {
                    public: __NEWLINE__
                        #public_functions_main_api
                    #fields_main_api
                };
                __NEWLINE__
            },
        }
    };
    let cc_details = {
        let mut prereqs = CcPrerequisites::default();
        let public_functions_cc_details = public_functions_cc_details.into_tokens(&mut prereqs);
        let fields_cc_details = fields_cc_details.into_tokens(&mut prereqs);
        prereqs.defs.insert(local_def_id);
        CcSnippet {
            prereqs,
            tokens: quote! {
                __NEWLINE__
                static_assert(
                    sizeof(#adt_cc_name) == #size,
                    "Verify that ADT layout didn't change since this header got generated");
                static_assert(
                    alignof(#adt_cc_name) == #alignment,
                    "Verify that ADT layout didn't change since this header got generated");
                __NEWLINE__
                #public_functions_cc_details
                #fields_cc_details
            },
        }
    };
    let rs_details = {
        let adt_rs_name = &core.rs_fully_qualified_name;
        quote! {
            const _: () = assert!(::std::mem::size_of::<#adt_rs_name>() == #size);
            const _: () = assert!(::std::mem::align_of::<#adt_rs_name>() == #alignment);
            #public_functions_rs_details
            #fields_rs_details
        }
    };
    ApiSnippets { main_api, cc_details, rs_details }
}

/// Formats the forward declaration of an algebraic data type (an ADT - a
/// struct, an enum, or a union), returning something like
/// `quote!{ struct SomeStruct; }`.
///
/// Will panic if `def_id` doesn't identify an ADT that can be successfully
/// handled by `format_adt_core`.
fn format_fwd_decl(db: &Database<'_>, def_id: LocalDefId) -> TokenStream {
    let def_id = def_id.to_def_id(); // LocalDefId -> DefId conversion.

    // `format_fwd_decl` should only be called for items from
    // `CcPrerequisites::fwd_decls` and `fwd_decls` should only contain ADTs
    // that `format_adt_core` succeeds for.
    let core_bindings = db
        .format_adt_core(def_id)
        .expect("`format_fwd_decl` should only be called if `format_adt_core` succeeded");
    let AdtCoreBindings { keyword, cc_short_name, .. } = &*core_bindings;

    quote! { #keyword #cc_short_name; }
}

fn format_source_location(tcx: TyCtxt, local_def_id: LocalDefId) -> String {
    let def_span = tcx.def_span(local_def_id);
    let rustc_span::FileLines { file, lines } =
        match tcx.sess().source_map().span_to_lines(def_span) {
            Ok(filelines) => filelines,
            Err(_) => return "unknown location".to_string(),
        };
    let file_name = file.name.prefer_local().to_string();
    // Note: line_index starts at 0, while CodeSearch starts indexing at 1.
    let line_number = lines[0].line_index + 1;
    let google3_prefix = {
        // If rustc_span::FileName isn't a 'real' file, then it's surrounded by by angle
        // brackets, thus don't prepend "google3/" prefix.
        if file.name.is_real() { "google3/" } else { "" }
    };
    format!("{google3_prefix}{file_name};l={line_number}")
}

/// Formats the doc comment (if any) associated with the item identified by
/// `local_def_id`, and appends the source location at which the item is
/// defined.
fn format_doc_comment(tcx: TyCtxt, local_def_id: LocalDefId) -> TokenStream {
    let hir_id = tcx.local_def_id_to_hir_id(local_def_id);
    let doc_comment = tcx
        .hir()
        .attrs(hir_id)
        .iter()
        .filter_map(|attr| attr.doc_str())
        .map(|symbol| symbol.to_string())
        .chain(once(format!("Generated from: {}", format_source_location(tcx, local_def_id))))
        .join("\n\n");
    quote! { __COMMENT__ #doc_comment}
}

/// Formats a HIR item idenfied by `def_id`.  Returns `None` if the item
/// can be ignored. Returns an `Err` if the definition couldn't be formatted.
///
/// Will panic if `def_id` is invalid (i.e. doesn't identify a HIR item).
fn format_item(db: &dyn BindingsGenerator<'_>, def_id: LocalDefId) -> Result<Option<ApiSnippets>> {
    let tcx = db.tcx();
    // TODO(b/262052635): When adding support for re-exports we may need to change
    // `is_directly_public` below into `is_exported`.  (OTOH such change *alone* is
    // undesirable, because it would mean exposing items from a private module.
    // Exposing a private module is undesirable, because it would mean that
    // changes of private implementation details of the crate could become
    // breaking changes for users of the generated C++ bindings.)
    if !tcx.effective_visibilities(()).is_directly_public(def_id) {
        return Ok(None);
    }

    match tcx.hir().expect_item(def_id) {
        Item { kind: ItemKind::Struct(_, generics) |
                     ItemKind::Enum(_, generics) |
                     ItemKind::Union(_, generics),
               .. } if !generics.params.is_empty() => {
            bail!("Generic types are not supported yet (b/259749095)");
        },
        Item { kind: ItemKind::Fn(..), .. } => db.format_fn(def_id).map(Some),
        Item { kind: ItemKind::Struct(..) | ItemKind::Enum(..) | ItemKind::Union(..), .. } =>
            db.format_adt_core(def_id.to_def_id())
                .map(|core| Some(format_adt(db, core))),
        Item { kind: ItemKind::TyAlias(..), ..} => format_type_alias(db, def_id).map(Some),
        Item { ident, kind: ItemKind::Use(use_path, use_kind), ..} => {
            format_use(db, ident.as_str(), use_path, use_kind).map(Some)
        },
        Item { kind: ItemKind::Impl(_), .. } |  // Handled by `format_adt`
        Item { kind: ItemKind::Mod(_), .. } =>  // Handled by `format_crate`
            Ok(None),
        Item { kind, .. } => bail!("Unsupported rustc_hir::hir::ItemKind: {}", kind.descr()),
    }
}

/// Formats a C++ comment explaining why no bindings have been generated for
/// `local_def_id`.
fn format_unsupported_def(
    db: &dyn BindingsGenerator<'_>,
    local_def_id: LocalDefId,
    err: Error,
) -> ApiSnippets {
    let tcx = db.tcx();
    db.errors().insert(&err);
    let source_loc = format_source_location(tcx, local_def_id);
    let name = tcx.def_path_str(local_def_id.to_def_id());

    // https://docs.rs/anyhow/latest/anyhow/struct.Error.html#display-representations
    // says: To print causes as well [...], use the alternate selector “{:#}”.
    let msg = format!("Error generating bindings for `{name}` defined at {source_loc}: {err:#}");
    let main_api = CcSnippet::new(quote! { __NEWLINE__ __NEWLINE__ __COMMENT__ #msg __NEWLINE__ });

    ApiSnippets { main_api, cc_details: CcSnippet::default(), rs_details: quote! {} }
}

/// Formats namespace-bound snippets, given an iterator over (namespace_def_id,
/// namespace_qualifier, tokens) and the TyCtxt.
///
/// (The namespace_def_id is optional, where None corresponds to the top-level
/// namespace.)
///
/// For example, `[(id, ns, tokens)]` will be formatted as:
///
///     ```
///     namespace ns {
///     #tokens
///     }
///     ```
///
/// `format_namespace_bound_cc_tokens` tries to give a nice-looking output - for
/// example it combines consecutive items that belong to the same namespace,
/// when given `[(id, ns, tokens1), (id, ns, tokens2)]` as input:
///
///     ```
///     namespace ns {
///     #tokens1
///     #tokens2
///     }
///     ```
///
/// `format_namespace_bound_cc_tokens` also knows that top-level items (e.g.
/// ones where `NamespaceQualifier` doesn't contain any namespace names) should
/// be emitted at the top-level (not nesting them under a `namespace` keyword).
/// For example, `[(None, toplevel_ns, tokens)]` will be formatted as just:
///
///     ```
///     #tokens
///     ```
pub fn format_namespace_bound_cc_tokens(
    iter: impl IntoIterator<Item = (Option<DefId>, NamespaceQualifier, TokenStream)>,
    tcx: TyCtxt,
) -> TokenStream {
    let iter = iter
        .into_iter()
        .coalesce(|(id1, ns1, mut tokens1), (id2, ns2, tokens2)| {
            // Coalesce tokens if consecutive items belong to the same namespace.
            if (id1 == id2) && (ns1 == ns2) {
                tokens1.extend(tokens2);
                Ok((id1, ns1, tokens1))
            } else {
                Err(((id1, ns1, tokens1), (id2, ns2, tokens2)))
            }
        })
        .map(|(ns_def_id_opt, ns, tokens)| {
            let mut ns_attributes = vec![];
            if let Some(ns_def_id) = ns_def_id_opt {
                if let Some(cc_deprecated_tag) = format_deprecated_tag(tcx, ns_def_id) {
                    ns_attributes.push(cc_deprecated_tag);
                }
            }
            ns.format_with_cc_body(tokens, ns_attributes).unwrap_or_else(|err| {
                let name = ns.0.iter().join("::");
                let err = format!("Failed to format namespace name `{name}`: {err}");
                quote! { __COMMENT__ #err }
            })
        });

    // Using fully-qualified syntax to avoid the warning that `intersperse`
    // may be added to the standard library in the future.
    //
    // TODO(https://github.com/rust-lang/rust/issues/79524): Use `.intersperse(...)` syntax once
    // 1) this stdlib feature gets stabilized and
    // 2) the method with conflicting name gets removed from `itertools`.
    let iter = itertools::Itertools::intersperse(iter, quote! { __NEWLINE__ __NEWLINE__ });

    iter.collect()
}

/// Formats all public items from the Rust crate being compiled.
fn format_crate(db: &Database) -> Result<Output> {
    let tcx = db.tcx();
    let mut cc_details_prereqs = CcPrerequisites::default();
    let mut cc_details: Vec<(LocalDefId, TokenStream)> = vec![];
    let mut rs_body = TokenStream::default();
    let mut main_apis = HashMap::<LocalDefId, CcSnippet>::new();
    let formatted_items = tcx
        .hir()
        .items()
        .filter_map(|item_id| {
            let def_id: LocalDefId = item_id.owner_id.def_id;
            db.format_item(def_id)
                .unwrap_or_else(|err| Some(format_unsupported_def(db, def_id, err)))
                .map(|api_snippets| (def_id, api_snippets))
        })
        .sorted_by_key(|(def_id, _)| tcx.def_span(*def_id));
    for (def_id, api_snippets) in formatted_items {
        let old_item = main_apis.insert(def_id, api_snippets.main_api);
        assert!(old_item.is_none(), "Duplicated key: {def_id:?}");

        // `cc_details` don't participate in the toposort, because
        // `CcPrerequisites::defs` always use `main_api` as the predecessor
        // - `chain`ing `cc_details` after `ordered_main_apis` trivially
        // meets the prerequisites.
        cc_details.push((def_id, api_snippets.cc_details.into_tokens(&mut cc_details_prereqs)));
        rs_body.extend(api_snippets.rs_details);
    }

    // Find the order of `main_apis` that 1) meets the requirements of
    // `CcPrerequisites::defs` and 2) makes a best effort attempt to keep the
    // `main_apis` in the same order as the source order of the Rust APIs.
    let ordered_ids = {
        let toposort::TopoSortResult { ordered: ordered_ids, failed: failed_ids } = {
            let nodes = main_apis.keys().copied();
            let deps = main_apis.iter().flat_map(|(&successor, main_api)| {
                let predecessors = main_api.prereqs.defs.iter().copied();
                predecessors.map(move |predecessor| toposort::Dependency { predecessor, successor })
            });
            toposort::toposort(nodes, deps, move |lhs_id, rhs_id| {
                tcx.def_span(*lhs_id).cmp(&tcx.def_span(*rhs_id))
            })
        };
        assert_eq!(
            0,
            failed_ids.len(),
            "There are no known scenarios where CcPrerequisites::defs can form \
                    a dependency cycle. These `LocalDefId`s form an unexpected cycle: {}",
            failed_ids.into_iter().map(|id| format!("{:?}", id)).join(",")
        );
        ordered_ids
    };

    // Destructure/rebuild `main_apis` (in the same order as `ordered_ids`) into
    // `includes`, and `ordered_cc` (mixing in `fwd_decls` and `cc_details`).
    let (includes, ordered_cc) = {
        let mut already_declared = HashSet::new();
        let mut fwd_decls = HashSet::new();
        let mut includes = cc_details_prereqs.includes;
        let mut ordered_main_apis: Vec<(LocalDefId, TokenStream)> = Vec::new();
        for def_id in ordered_ids.into_iter() {
            let CcSnippet {
                tokens: cc_tokens,
                prereqs: CcPrerequisites {
                    includes: mut inner_includes,
                    fwd_decls: inner_fwd_decls,
                    .. // `defs` have already been utilized by `toposort` above
                }
            } = main_apis.remove(&def_id).unwrap();

            fwd_decls.extend(inner_fwd_decls.difference(&already_declared).copied());
            already_declared.insert(def_id);
            already_declared.extend(inner_fwd_decls.into_iter());

            includes.append(&mut inner_includes);
            ordered_main_apis.push((def_id, cc_tokens));
        }

        let fwd_decls = fwd_decls
            .into_iter()
            .sorted_by_key(|def_id| tcx.def_span(*def_id))
            .map(|local_def_id| (local_def_id, format_fwd_decl(db, local_def_id)));

        // The first item of the tuple here is the DefId of the namespace.
        let ordered_cc: Vec<(Option<DefId>, NamespaceQualifier, TokenStream)> = fwd_decls
            .into_iter()
            .chain(ordered_main_apis)
            .chain(cc_details)
            .map(|(local_def_id, tokens)| {
                let ns_def_id = tcx.opt_parent(local_def_id.to_def_id());
                let mod_path = FullyQualifiedName::new(tcx, local_def_id.to_def_id()).mod_path;
                (ns_def_id, mod_path, tokens)
            })
            .collect_vec();

        (includes, ordered_cc)
    };

    // Generate top-level elements of the C++ header file.
    let h_body = {
        // TODO(b/254690602): Decide whether using `#crate_name` as the name of the
        // top-level namespace is okay (e.g. investigate if this name is globally
        // unique + ergonomic).
        let crate_name = format_cc_ident(tcx.crate_name(LOCAL_CRATE).as_str())?;

        let includes = format_cc_includes(&includes);
        let ordered_cc = format_namespace_bound_cc_tokens(ordered_cc, tcx);
        quote! {
            #includes
            __NEWLINE__ __NEWLINE__
            namespace #crate_name {
                __NEWLINE__
                #ordered_cc
                __NEWLINE__
            }
            __NEWLINE__
        }
    };

    Ok(Output { h_body, rs_body })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    use quote::quote;

    use error_report::IgnoreErrors;
    use run_compiler_test_support::{find_def_id_by_name, run_compiler_for_testing};
    use token_stream_matchers::{
        assert_cc_matches, assert_cc_not_matches, assert_rs_matches, assert_rs_not_matches,
    };

    /// This test covers only a single example of a function that should get a
    /// C++ binding. The test focuses on verification that the output from
    /// `format_fn` gets propagated all the way to `GenerateBindings::new`.
    /// Additional coverage of how functions are formatted is provided
    /// by `test_format_item_..._fn_...` tests (which work at the `format_fn`
    /// level).
    #[test]
    fn test_generated_bindings_fn_no_mangle_extern_c() {
        let test_src = r#"
                #[no_mangle]
                pub extern "C" fn public_function() {
                    println!("foo");
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    extern "C" void public_function();
                }
            );

            // No Rust thunks should be generated in this test scenario.
            assert_rs_not_matches!(bindings.rs_body, quote! { public_function });
        });
    }

    /// `test_generated_bindings_fn_export_name` covers a scenario where
    /// `MixedSnippet::cc` is present but `MixedSnippet::rs` is empty
    /// (because no Rust thunks are needed).
    #[test]
    fn test_generated_bindings_fn_export_name() {
        let test_src = r#"
                #[export_name = "export_name"]
                pub extern "C" fn public_function(x: f64, y: f64) -> f64 { x + y }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    namespace rust_out {
                        ...
                        double public_function(double x, double y);
                        namespace __crubit_internal {
                            extern "C" double export_name(double, double);
                        }
                        inline double public_function(double x, double y) {
                            return __crubit_internal::export_name(x, y);
                        }
                    }
                }
            );
        });
    }

    /// The `test_generated_bindings_struct` test covers only a single example
    /// of an ADT (struct/enum/union) that should get a C++ binding.
    /// Additional coverage of how items are formatted is provided by
    /// `test_format_item_..._struct_...`, `test_format_item_..._enum_...`,
    /// and `test_format_item_..._union_...` tests.
    ///
    /// We don't want to duplicate coverage already provided by
    /// `test_format_item_struct_with_fields`, but we do want to verify that
    /// * `format_crate` will actually find and process the struct
    ///   (`test_format_item_...` doesn't cover this aspect - it uses a
    ///   test-only `find_def_id_by_name` instead)
    /// * The actual shape of the bindings still looks okay at this level.
    #[test]
    fn test_generated_bindings_struct() {
        let test_src = r#"
                pub struct Point {
                    pub x: i32,
                    pub y: i32,
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    namespace rust_out {
                        ...
                        struct CRUBIT_INTERNAL_RUST_TYPE(":: rust_out :: Point") alignas(4) [[clang::trivial_abi]] Point final {
                            // No point replicating test coverage of
                            // `test_format_item_struct_with_fields`.
                            ...
                        };
                        static_assert(sizeof(Point) == 8, ...);
                        static_assert(alignof(Point) == 4, ...);
                        ... // Other static_asserts are covered by
                            // `test_format_item_struct_with_fields`
                    }  // namespace rust_out
                }
            );
            assert_rs_matches!(
                bindings.rs_body,
                quote! {
                    // No point replicating test coverage of
                    // `test_format_item_struct_with_fields`.
                    const _: () = assert!(::std::mem::size_of::<::rust_out::Point>() == 8);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::Point>() == 4);
                    const _: () = assert!(::core::mem::offset_of!(::rust_out::Point, x) == 0);
                    const _: () = assert!(::core::mem::offset_of!(::rust_out::Point, y) == 4);
                }
            );
        });
    }

    /// The `test_generated_bindings_impl` test covers only a single example of
    /// a non-trait `impl`. Additional coverage of how items are formatted
    /// should be provided in the future by `test_format_item_...` tests.
    ///
    /// We don't want to duplicate coverage already provided by
    /// `test_format_item_static_method`, but we do want to verify that
    /// * `format_crate` won't process the `impl` as a standalone HIR item
    /// * The actual shape of the bindings still looks okay at this level.
    #[test]
    fn test_generated_bindings_impl() {
        let test_src = r#"
                #![allow(dead_code)]

                pub struct SomeStruct(i32);

                impl SomeStruct {
                    pub fn public_static_method() -> i32 { 123 }

                    fn private_static_method() -> i32 { 123 }
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    namespace rust_out {
                        ...
                        struct ... SomeStruct ... {
                            // No point replicating test coverage of
                            // `test_format_item_static_method`.
                            ...
                            std::int32_t public_static_method();
                            ...
                        };
                        ...
                        std::int32_t SomeStruct::public_static_method() {
                            ...
                        }
                        ...
                    }  // namespace rust_out
                }
            );
            assert_rs_matches!(
                bindings.rs_body,
                quote! {
                    extern "C" fn ...() -> i32 {
                        ::rust_out::SomeStruct::public_static_method()
                    }
                }
            );
        });
    }

    #[test]
    fn test_generated_bindings_includes() {
        let test_src = r#"
                #[no_mangle]
                pub extern "C" fn public_function(i: i32, d: isize, u: u64) {
                    dbg!(i);
                    dbg!(d);
                    dbg!(u);
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    __HASH_TOKEN__ include <cstdint> ...
                    namespace ... {
                        ...
                        extern "C" void public_function(
                            std::int32_t i,
                            std::intptr_t d,
                            std::uint64_t u);
                    }
                }
            );
        });
    }

    /// Tests that `toposort` is used to reorder item bindings.
    #[test]
    fn test_generated_bindings_prereq_defs_field_deps_require_reordering() {
        let test_src = r#"
                #![allow(dead_code)]

                // In the generated bindings `Outer` needs to come *after* `Inner`.
                pub struct Outer(Inner);
                pub struct Inner(bool);
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    namespace rust_out {
                    ...
                        struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(1) [[clang::trivial_abi]] Inner final {
                          ... union { ... bool __field0; }; ...
                        };
                    ...
                        struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(1) [[clang::trivial_abi]] Outer final {
                          ... union { ... ::rust_out::Inner __field0; }; ...
                        };
                    ...
                    }  // namespace rust_out
                }
            );
        });
    }

    /// Tests that a forward declaration is present when it is required to
    /// preserve the original source order.  In this test the
    /// `CcPrerequisites::fwd_decls` dependency comes from a pointer parameter.
    #[test]
    fn test_generated_bindings_prereq_fwd_decls_for_ptr_param() {
        let test_src = r#"
                #![allow(dead_code)]

                // To preserve original API order we need to forward declare S.
                pub fn f(_: *const S) {}
                pub struct S(bool);
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    namespace rust_out {
                        ...
                        // Verifing the presence of this forward declaration
                        // it the essence of this test.  The order of the items
                        // below also matters.
                        struct S;
                        ...
                        void f(::rust_out::S const* __param_0);
                        ...
                        struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(...) [[clang::trivial_abi]] S final { ... }
                        ...
                        inline void f(::rust_out::S const* __param_0) { ... }
                        ...
                    }  // namespace rust_out
                }
            );
        });
    }

    /// Tests that a forward declaration is present when it is required to
    /// preserve the original source order.  In this test the
    /// `CcPrerequisites::fwd_decls` dependency comes from a
    /// function declaration that has a parameter that takes a struct by value.
    #[test]
    fn test_generated_bindings_prereq_fwd_decls_for_cpp_fn_decl() {
        let test_src = r#"
                #[no_mangle]
                pub extern "C" fn f(s: S) -> bool { s.0 }

                #[repr(C)]
                pub struct S(bool);
            "#;

        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    namespace rust_out {
                        ...
                        // Verifing the presence of this forward declaration
                        // is the essence of this test.  The order also matters:
                        // 1. The fwd decl of `S` should come first,
                        // 2. Declaration of `f` and definition of `S` should come next
                        //    (in their original order - `f` first and then `S`).
                        struct S;
                        ...
                        // `CcPrerequisites` of `f` declaration below (the main api of `f`) should
                        // include `S` as a `fwd_decls` edge, rather than as a `defs` edge.
                        bool f(::rust_out::S s);
                        ...
                        struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(...) [[clang::trivial_abi]] S final { ... }
                        ...
                    }  // namespace rust_out
                }
            );
        });
    }

    /// This test verifies that a forward declaration for a given ADT is only
    /// emitted once (and not once for every API item that requires the
    /// forward declaration as a prerequisite).
    #[test]
    fn test_generated_bindings_prereq_fwd_decls_no_duplication() {
        let test_src = r#"
                #![allow(dead_code)]

                // All three functions below require a forward declaration of S.
                pub fn f1(_: *const S) {}
                pub fn f2(_: *const S) {}
                pub fn f3(_: *const S) {}

                pub struct S(bool);

                // This function also includes S in its CcPrerequisites::fwd_decls
                // (although here it is not required, because the definition of S
                // is already available above).
                pub fn f4(_: *const S) {}
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap().h_body.to_string();

            // Only a single forward declaration is expected.
            assert_eq!(1, bindings.matches("struct S ;").count(), "bindings = {bindings}");
        });
    }

    /// This test verifies that forward declarations are emitted in a
    /// deterministic order. The particular order doesn't matter _that_
    /// much, but it definitely shouldn't change every time
    /// `cc_bindings_from_rs` is invoked again.  The current order preserves
    /// the original source order of the Rust API items.
    #[test]
    fn test_generated_bindings_prereq_fwd_decls_deterministic_order() {
        let test_src = r#"
                #![allow(dead_code)]

                // To try to mix things up, the bindings for the functions below
                // will *ask* for forward declarations in a different order:
                // * Different from the order in which the forward declarations
                //   are expected to be *emitted* (the original source order).
                // * Different from alphabetical order.
                pub fn f1(_: *const b::S3) {}
                pub fn f2(_: *const a::S2) {}
                pub fn f3(_: *const a::S1) {}

                pub mod a {
                    pub struct S1(bool);
                    pub struct S2(bool);
                }

                pub mod b {
                    pub struct S3(bool);
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    namespace rust_out {
                        ...
                        // Verifying that we get the same order in each test
                        // run is the essence of this test.
                        namespace a {
                        struct S1;
                        struct S2;
                        }
                        namespace b {
                        struct S3;
                        }
                        ...
                        void f1 ...
                        void f2 ...
                        void f3 ...

                        namespace a { ...
                        struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(...) [[clang::trivial_abi]] S1 final { ... } ...
                        struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(...) [[clang::trivial_abi]] S2 final { ... } ...
                        } ...
                        namespace b { ...
                        struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(...) [[clang::trivial_abi]] S3 final { ... } ...
                        } ...
                    }  // namespace rust_out
                }
            );
        });
    }

    /// This test verifies that forward declarations are not emitted if they are
    /// not needed (e.g. if bindings the given `struct` or other ADT have
    /// already been defined earlier).  In particular, we don't want to emit
    /// forward declarations for *all* `structs` (regardless if they are
    /// needed or not).
    #[test]
    fn test_generated_bindings_prereq_fwd_decls_not_needed_because_of_initial_order() {
        let test_src = r#"
                #[allow(dead_code)]

                pub struct S(bool);

                // S is already defined above - no need for forward declaration in C++.
                pub fn f(_s: *const S) {}
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_not_matches!(bindings.h_body, quote! { struct S; });
            assert_cc_matches!(bindings.h_body, quote! { void f(::rust_out::S const* _s); });
        });
    }

    /// This test verifies that a method declaration doesn't ask for a forward
    /// declaration to the struct.
    #[test]
    fn test_generated_bindings_prereq_fwd_decls_not_needed_inside_struct_definition() {
        let test_src = r#"
                #![allow(dead_code)]

                pub struct S {
                    // This shouldn't require a fwd decl of S.
                    field: *const S,
                }

                impl S {
                    // This shouldn't require a fwd decl of S.
                    pub fn create() -> S { Self{ field: std::ptr::null() } }
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_not_matches!(bindings.h_body, quote! { struct S; });
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    static ::rust_out::S create(); ...
                    union { ... ::rust_out::S const* field; }; ...
                }
            );
        });
    }

    #[test]
    fn test_generated_bindings_module_basics() {
        let test_src = r#"
                pub mod some_module {
                    pub fn some_func() {}
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    namespace rust_out {
                        namespace some_module {
                            ...
                            inline void some_func() { ... }
                            ...
                        }  // namespace some_module
                    }  // namespace rust_out
                }
            );
            assert_rs_matches!(
                bindings.rs_body,
                quote! {
                    #[no_mangle]
                    extern "C"
                    fn ...() -> () {
                        ::rust_out::some_module::some_func()
                    }
                }
            );
        });
    }

    #[test]
    fn test_generated_bindings_module_name_is_cpp_reserved_keyword() {
        let test_src = r#"
                pub mod working_module {
                    pub fn working_module_f1() {}
                    pub fn working_module_f2() {}
                }
                pub mod reinterpret_cast {
                    pub fn broken_module_f1() {}
                    pub fn broken_module_f2() {}
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();

            // Items in the broken module should be replaced with a comment explaining the
            // problem.
            let broken_module_msg = "Failed to format namespace name `reinterpret_cast`: \
                                     `reinterpret_cast` is a C++ reserved keyword \
                                     and can't be used as a C++ identifier";
            assert_cc_not_matches!(bindings.h_body, quote! { namespace reinterpret_cast });
            assert_cc_not_matches!(bindings.h_body, quote! { broken_module_f1 });
            assert_cc_not_matches!(bindings.h_body, quote! { broken_module_f2 });

            // Items in the other module should still go through.
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    namespace rust_out {
                        namespace working_module {
                            ...
                            void working_module_f1();
                            ...
                            void working_module_f2();
                            ...
                        }  // namespace some_module

                        __COMMENT__ #broken_module_msg
                        ...
                    }  // namespace rust_out
                }
            );
        });
    }

    /// `test_generated_bindings_non_pub_items` verifies that non-public items
    /// are not present/propagated into the generated bindings.
    #[test]
    fn test_generated_bindings_non_pub_items() {
        let test_src = r#"
                #![allow(dead_code)]

                extern "C" fn private_function() {
                    println!("foo");
                }

                struct PrivateStruct {
                    x: i32,
                    y: i32,
                }

                pub struct PublicStruct(i32);

                impl PublicStruct {
                    fn private_method() {}
                }

                pub mod public_module {
                    fn priv_func_in_pub_module() {}
                }

                mod private_module {
                    pub fn pub_func_in_priv_module() { priv_func_in_priv_module() }
                    fn priv_func_in_priv_module() {}
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_not_matches!(bindings.h_body, quote! { private_function });
            assert_rs_not_matches!(bindings.rs_body, quote! { private_function });
            assert_cc_not_matches!(bindings.h_body, quote! { PrivateStruct });
            assert_rs_not_matches!(bindings.rs_body, quote! { PrivateStruct });
            assert_cc_not_matches!(bindings.h_body, quote! { private_method });
            assert_rs_not_matches!(bindings.rs_body, quote! { private_method });
            assert_cc_not_matches!(bindings.h_body, quote! { priv_func_in_priv_module });
            assert_rs_not_matches!(bindings.rs_body, quote! { priv_func_in_priv_module });
            assert_cc_not_matches!(bindings.h_body, quote! { priv_func_in_pub_module });
            assert_rs_not_matches!(bindings.rs_body, quote! { priv_func_in_pub_module });
            assert_cc_not_matches!(bindings.h_body, quote! { private_module });
            assert_rs_not_matches!(bindings.rs_body, quote! { private_module });
            assert_cc_not_matches!(bindings.h_body, quote! { pub_func_in_priv_module });
            assert_rs_not_matches!(bindings.rs_body, quote! { pub_func_in_priv_module });
        });
    }

    #[test]
    fn test_generated_bindings_top_level_items() {
        let test_src = "pub fn public_function() {}";
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            let expected_comment_txt = "Automatically @generated C++ bindings for the following Rust crate:\n\
                 rust_out";
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    __COMMENT__ #expected_comment_txt
                    ...
                    __HASH_TOKEN__ pragma once
                    ...
                    namespace rust_out {
                        ...
                    }
                }
            );
            assert_cc_matches!(
                bindings.rs_body,
                quote! {
                    __COMMENT__ #expected_comment_txt
                }
            );
        })
    }

    /// The `test_generated_bindings_unsupported_item` test verifies how `Err`
    /// from `format_item` is formatted as a C++ comment (in `format_crate`
    /// and `format_unsupported_def`):
    /// - This test covers only a single example of an unsupported item.
    ///   Additional coverage is provided by `test_format_item_unsupported_...`
    ///   tests.
    /// - This test somewhat arbitrarily chooses an example of an unsupported
    ///   item, trying to pick one that 1) will never be supported (b/254104998
    ///   has some extra notes about APIs named after reserved C++ keywords) and
    ///   2) tests that the full error chain is included in the message.
    #[test]
    fn test_generated_bindings_unsupported_item() {
        let test_src = r#"
                #[no_mangle]
                pub extern "C" fn reinterpret_cast() {}
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            let expected_comment_txt = "Error generating bindings for `reinterpret_cast` \
                 defined at <crubit_unittests.rs>;l=3: \
                 Error formatting function name: \
                 `reinterpret_cast` is a C++ reserved keyword \
                 and can't be used as a C++ identifier";
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    __COMMENT__ #expected_comment_txt
                }
            );
        })
    }

    #[test]
    fn test_generated_bindings_reimports() {
        let test_src = r#"
                #![allow(dead_code)]
                #![allow(unused_imports)]
                mod private_submodule1 {
                    pub fn subfunction1() {}
                    pub fn subfunction2() {}
                    pub fn subfunction3() {}
                }
                mod private_submodule2 {
                    pub fn subfunction8() {}
                    pub fn subfunction9() {}
                }

                // Public re-import.
                pub use private_submodule1::subfunction1;

                // Private re-import.
                use private_submodule1::subfunction2;

                // Re-import that renames.
                pub use private_submodule1::subfunction3 as public_function3;

                // Re-import of multiple items via glob.
                pub use private_submodule2::*;
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();

            let failures = vec![(1, 15), (3, 21)];
            for (use_number, line_number) in failures.into_iter() {
                let expected_comment_txt = format!(
                    "Error generating bindings for `{{use#{use_number}}}` defined at \
                     <crubit_unittests.rs>;l={line_number}: \
                     Not directly public type (re-exports are not supported yet - b/262052635)"
                );
                assert_cc_matches!(
                    bindings.h_body,
                    quote! {
                        __COMMENT__ #expected_comment_txt
                    }
                );
            }
        });
    }

    #[test]
    fn test_generated_bindings_module_deprecated_no_args() {
        let test_src = r#"
                #[deprecated]
                pub mod some_module {
                    pub fn some_function() {}
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    ...
                        [[deprecated]]
                        namespace some_module {
                            ...
                        }  // namespace some_module
                    ...
                }
            );
        });
    }

    #[test]
    fn test_generated_bindings_module_deprecated_with_message() {
        let test_src = r#"
                #[deprecated = "Use other_module instead"]
                pub mod some_module {
                    pub fn some_function() {}
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    ...
                        [[deprecated("Use other_module instead")]]
                        namespace some_module {
                            ...
                        }  // namespace some_module
                    ...
                }
            );
        });
    }

    #[test]
    fn test_generated_bindings_module_deprecated_named_args() {
        let test_src = r#"
                #[deprecated(since = "3.14", note = "Use other_module instead")]
                pub mod some_module {
                    pub fn some_function() {}
                }
            "#;
        test_generated_bindings(test_src, |bindings| {
            let bindings = bindings.unwrap();
            assert_cc_matches!(
                bindings.h_body,
                quote! {
                    ...
                        [[deprecated("Use other_module instead")]]
                        namespace some_module {
                            ...
                        }  // namespace some_module
                    ...
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_extern_c_no_mangle_no_params_no_return_type() {
        let test_src = r#"
                #[no_mangle]
                pub extern "C" fn public_function() {}
            "#;
        test_format_item(test_src, "public_function", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    extern "C" void public_function();
                }
            );

            // Sufficient to just re-declare the Rust API in C++.
            // (i.e. there is no need to have a C++-side definition of `public_function`).
            assert!(result.cc_details.tokens.is_empty());

            // There is no need to have a separate thunk for an `extern "C"` function.
            assert!(result.rs_details.is_empty());
        });
    }

    /// The `test_format_item_fn_explicit_unit_return_type` test below is very
    /// similar to the
    /// `test_format_item_fn_extern_c_no_mangle_no_params_no_return_type` above,
    /// except that the return type is explicitly spelled out.  There is no
    /// difference in `ty::FnSig` so our code behaves exactly the same, but the
    /// test has been planned based on earlier, hir-focused approach and having
    /// this extra test coverage shouldn't hurt. (`hir::FnSig`
    /// and `hir::FnRetTy` _would_ see a difference between the two tests, even
    /// though there is no different in the current `bindings.rs` code).
    #[test]
    fn test_format_item_fn_explicit_unit_return_type() {
        let test_src = r#"
                #[no_mangle]
                pub extern "C" fn explicit_unit_return_type() -> () {}
            "#;
        test_format_item(test_src, "explicit_unit_return_type", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    extern "C" void explicit_unit_return_type();
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_never_return_type() {
        let test_src = r#"
                #[no_mangle]
                pub extern "C" fn never_returning_function() -> ! {
                    panic!("This function panics and therefore never returns");
                }
            "#;
        test_format_item(test_src, "never_returning_function", |result| {
            // TODO(b/254507801): The function should be annotated with the `[[noreturn]]`
            // attribute.
            // TODO(b/254507801): Expect `crubit::Never` instead (see the bug for more
            // details).
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    extern "C" void never_returning_function();
                }
            );
        })
    }

    /// `test_format_item_fn_mangling` checks that bindings can be generated for
    /// `extern "C"` functions that do *not* have `#[no_mangle]` attribute.  The
    /// test elides away the mangled name in the `assert_cc_matches` checks
    /// below, but end-to-end test coverage should eventually be provided by
    /// `test/functions` (see b/262904507).
    #[test]
    fn test_format_item_fn_mangling() {
        let test_src = r#"
                pub extern "C" fn public_function(x: f64, y: f64) -> f64 { x + y }
            "#;
        test_format_item(test_src, "public_function", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    double public_function(double x, double y);
                }
            );
            // TODO(b/262904507): omit the thunk and uncomment the next line.
            // assert!(result.rs_details.is_empty());
            assert!(result.cc_details.prereqs.is_empty());
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" double ...(double, double);
                    }
                    ...
                    inline double public_function(double x, double y) {
                        return __crubit_internal::...(x, y);
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_export_name() {
        let test_src = r#"
                #[export_name = "export_name"]
                pub extern "C" fn public_function(x: f64, y: f64) -> f64 { x + y }
            "#;
        test_format_item(test_src, "public_function", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    double public_function(double x, double y);
                }
            );

            // There is no need to have a separate thunk for an `extern "C"` function.
            assert!(result.rs_details.is_empty());

            // We generate a C++-side definition of `public_function` so that we
            // can call a differently-named (but same-signature) `export_name` function.
            assert!(result.cc_details.prereqs.is_empty());
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" double export_name(double, double);
                    }
                    ...
                    inline double public_function(double x, double y) {
                        return __crubit_internal::export_name(x, y);
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_extern_c_unsafe() {
        let test_src = r#"
                #[no_mangle]
                pub unsafe extern "C" fn foo() {}
            "#;
        test_format_item(test_src, "foo", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    void foo();
                }
            );
            assert!(result.rs_details.is_empty());
        });
    }

    /// For non-extern "C" unsafe functions, we need a thunk, and it needs some
    /// `unsafe`.
    ///
    /// The thunk itself needs to be unsafe, because it wraps an unsafe function
    /// and is still in-principle itself directly callable. It also needs to
    /// have an unsafe block inside of it due to RFC #2585
    /// `unsafe_block_in_unsafe_fn`.
    #[test]
    fn test_format_item_fn_unsafe() {
        let test_src = r#"
                #[no_mangle]
                pub unsafe fn foo() {}
            "#;
        test_format_item(test_src, "foo", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    void foo();
                }
            );
            assert_cc_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    unsafe extern "C" fn __crubit_thunk_foo() -> () {
                        unsafe { ::rust_out::foo() }
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_fn_cpp_name() {
        let test_src = r#"
                #![feature(register_tool)]
                #![register_tool(__crubit)]

                #[no_mangle]
                #[__crubit::annotate(cpp_name="Create")]
                pub fn foo() {}
            "#;
        test_format_item(test_src, "foo", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());

            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn __crubit_thunk_foo() -> () {
                         ::rust_out::foo()
                    }
                }
            );

            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    void Create();
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" void __crubit_thunk_foo();
                    }
                    ...
                    inline void Create() {
                        return __crubit_internal::__crubit_thunk_foo();
                    }
                }
            );
        });
    }

    /// `test_format_item_fn_const` tests how bindings for an `const fn` are
    /// generated.
    ///
    /// Right now the `const` qualifier is ignored, but one can imagine that in
    /// the (very) long-term future such functions (including their bodies)
    /// could be translated into C++ `consteval` functions.
    #[test]
    fn test_format_item_fn_const() {
        let test_src = r#"
                pub const fn foo(i: i32) -> i32 { i * 42 }
            "#;
        test_format_item(test_src, "foo", |result| {
            // TODO(b/254095787): Update test expectations below once `const fn` from Rust
            // is translated into a `consteval` C++ function.
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    std::int32_t foo(std::int32_t i);
                }
            );
            assert!(!result.cc_details.prereqs.is_empty());
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" std::int32_t ...( std::int32_t);
                    }
                    ...
                    inline std::int32_t foo(std::int32_t i) {
                        return __crubit_internal::...(i);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C"
                    fn ...(i: i32) -> i32 {
                        ::rust_out::foo(i)
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_with_c_unwind_abi() {
        // See also https://rust-lang.github.io/rfcs/2945-c-unwind-abi.html
        let test_src = r#"
                #![feature(c_unwind)]

                #[no_mangle]
                pub extern "C-unwind" fn may_throw() {}
            "#;
        test_format_item(test_src, "may_throw", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    extern "C" void may_throw();
                }
            );
        });
    }

    /// This test mainly verifies that `format_item` correctly propagates
    /// `CcPrerequisites` of parameter types and return type.
    #[test]
    fn test_format_item_fn_cc_prerequisites_if_cpp_definition_needed() {
        let test_src = r#"
                #![allow(dead_code)]

                pub fn foo(_i: i32) -> S { panic!("foo") }
                pub struct S(i32);
            "#;
        test_format_item(test_src, "foo", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;

            // Minimal coverage, just to double-check that the test setup works.
            //
            // Note that this is a definition, and therefore `S` should be defined
            // earlier (not just forward declared).
            assert_cc_matches!(main_api.tokens, quote! { S foo(std::int32_t _i);});
            assert_cc_matches!(result.cc_details.tokens, quote! { S foo(std::int32_t _i) { ... }});

            // Main checks: `CcPrerequisites::includes`.
            assert_cc_matches!(
                format_cc_includes(&main_api.prereqs.includes),
                quote! { include <cstdint> }
            );
            assert_cc_matches!(
                format_cc_includes(&result.cc_details.prereqs.includes),
                quote! { include <cstdint> }
            );

            // Main checks: `CcPrerequisites::defs` and `CcPrerequisites::fwd_decls`.
            //
            // Verifying the actual def_id is tricky, because `test_format_item` doesn't
            // expose `tcx` to the verification function (and therefore calling
            // `find_def_id_by_name` is not easily possible).
            //
            // Note that `main_api` and `impl_details` have different expectations.
            assert_eq!(0, main_api.prereqs.defs.len());
            assert_eq!(1, main_api.prereqs.fwd_decls.len());
            assert_eq!(1, result.cc_details.prereqs.defs.len());
            assert_eq!(0, result.cc_details.prereqs.fwd_decls.len());
        });
    }

    /// This test verifies that `format_item` uses `CcPrerequisites::fwd_decls`
    /// rather than `CcPrerequisites::defs` for function declarations in the
    /// `main_api`.
    #[test]
    fn test_format_item_fn_cc_prerequisites_if_only_cpp_declaration_needed() {
        let test_src = r#"
                #[no_mangle]
                pub extern "C" fn foo(s: S) -> bool { s.0 }

                #[repr(C)]
                pub struct S(bool);
            "#;
        test_format_item(test_src, "foo", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;

            // Minimal coverage, just to double-check that the test setup works.
            //
            // Note that this is only a function *declaration* (not a function definition -
            // there is no function body), and therefore `S` just needs to be
            // forward-declared earlier.
            assert_cc_matches!(main_api.tokens, quote! { bool foo(::rust_out::S s); });

            // Main checks: `CcPrerequisites::defs` and `CcPrerequisites::fwd_decls`.
            //
            // Verifying the actual def_id is tricky, because `test_format_item` doesn't
            // expose `tcx` to the verification function (and therefore calling
            // `find_def_id_by_name` is not easily possible).
            assert_eq!(0, main_api.prereqs.defs.len());
            assert_eq!(1, main_api.prereqs.fwd_decls.len());
        });
    }

    #[test]
    fn test_format_item_fn_with_type_aliased_return_type() {
        // Type aliases disappear at the `rustc_middle::ty::Ty` level and therefore in
        // the short-term the generated bindings also ignore type aliases.
        //
        // TODO(b/254096006): Consider preserving `type` aliases when generating
        // bindings.
        let test_src = r#"
                type MyTypeAlias = f64;

                #[no_mangle]
                pub extern "C" fn type_aliased_return() -> MyTypeAlias { 42.0 }
            "#;
        test_format_item(test_src, "type_aliased_return", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    extern "C" double type_aliased_return();
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_with_doc_comment_with_unmangled_name() {
        let test_src = r#"
            /// Outer line doc.
            /** Outer block doc that spans lines.
             */
            #[doc = "Doc comment via doc attribute."]
            #[no_mangle]
            pub extern "C" fn fn_with_doc_comment_with_unmangled_name() {}
          "#;
        test_format_item(test_src, "fn_with_doc_comment_with_unmangled_name", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            let doc_comments = [
                " Outer line doc.",
                "",
                " Outer block doc that spans lines.",
                "             ",
                "",
                "Doc comment via doc attribute.",
                "",
                "Generated from: <crubit_unittests.rs>;l=7",
            ]
            .join("\n");
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    __COMMENT__ #doc_comments
                    extern "C" void fn_with_doc_comment_with_unmangled_name();
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_with_inner_doc_comment_with_unmangled_name() {
        let test_src = r#"
            /// Outer doc comment.
            #[no_mangle]
            pub extern "C" fn fn_with_inner_doc_comment_with_unmangled_name() {
                //! Inner doc comment.
            }
          "#;
        test_format_item(test_src, "fn_with_inner_doc_comment_with_unmangled_name", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            let doc_comments = [
                " Outer doc comment.",
                " Inner doc comment.",
                "Generated from: <crubit_unittests.rs>;l=4",
            ]
            .join("\n\n");
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    __COMMENT__ #doc_comments
                    extern "C" void fn_with_inner_doc_comment_with_unmangled_name();
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_with_doc_comment_with_mangled_name() {
        let test_src = r#"
                /// Doc comment of a function with mangled name.
                pub extern "C" fn fn_with_doc_comment_with_mangled_name() {}
            "#;
        test_format_item(test_src, "fn_with_doc_comment_with_mangled_name", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            let comment = " Doc comment of a function with mangled name.\n\n\
                           Generated from: <crubit_unittests.rs>;l=3";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    __COMMENT__ #comment
                    void fn_with_doc_comment_with_mangled_name();
                }
            );
        });
    }

    #[test]
    fn test_format_item_unsupported_fn_name_is_reserved_cpp_keyword() {
        let test_src = r#"
                #[no_mangle]
                pub extern "C" fn reinterpret_cast() -> () {}
            "#;
        test_format_item(test_src, "reinterpret_cast", |result| {
            let err = result.unwrap_err();
            assert_eq!(
                err,
                "Error formatting function name: \
                       `reinterpret_cast` is a C++ reserved keyword \
                       and can't be used as a C++ identifier"
            );
        });
    }

    #[test]
    fn test_format_item_unsupported_fn_ret_type() {
        let test_src = r#"
                pub fn foo() -> (i32, i32) { (123, 456) }
            "#;
        test_format_item(test_src, "foo", |result| {
            let err = result.unwrap_err();
            assert_eq!(
                err,
                "Error formatting function return type: \
                       Tuples are not supported yet: (i32, i32) (b/254099023)"
            );
        });
    }

    /// This test verifies handling of inferred, anonymous lifetimes.
    ///
    /// Note that `Region::get_name_or_anon()` may return the same name (e.g.
    /// `"anon"` for both lifetimes, but bindings should use 2 distinct
    /// lifetime names in the generated bindings and in the thunk impl.
    #[test]
    fn test_format_item_lifetime_generic_fn_with_inferred_lifetimes() {
        let test_src = r#"
                pub fn foo(arg: &i32) -> &i32 {
                    unimplemented!("arg = {arg}")
                }
            "#;
        test_format_item(test_src, "foo", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    std::int32_t const& [[clang::annotate_type("lifetime", "__anon1")]]
                    foo(std::int32_t const& [[clang::annotate_type("lifetime", "__anon1")]] arg);
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C"
                    std::int32_t const& [[clang::annotate_type("lifetime", "__anon1")]] ...(
                        std::int32_t const& [[clang::annotate_type("lifetime", "__anon1")]]);
                    }
                    inline
                    std::int32_t const& [[clang::annotate_type("lifetime", "__anon1")]]
                    foo(std::int32_t const& [[clang::annotate_type("lifetime", "__anon1")]] arg) {
                      return __crubit_internal::...(arg);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn ...<'__anon1>(arg: &'__anon1 i32) -> &'__anon1 i32 {
                        ::rust_out::foo(arg)
                    }
                }
            );
        });
    }

    /// This test verifies handling of various explicit (i.e. non-inferred)
    /// lifetimes.
    ///
    /// * Note that the two `'_` specify two distinct lifetimes (i.e. two
    ///   distinct names need to be used in the generated bindings and thunk
    ///   impl).
    /// * Note that `'static` doesn't need to be listed in the generic
    ///   parameters of the thunk impl
    /// * Note that even though `'foo` is used in 2 parameter types, it should
    ///   only appear once in the list of generic parameters of the thunk impl
    /// * Note that in the future the following translation may be preferable:
    ///     * `'a` => `$a` (no parens)
    ///     * `'foo` => `$(foo)` (note the extra parens)
    #[test]
    fn test_format_item_lifetime_generic_fn_with_various_lifetimes() {
        let test_src = r#"
                pub fn foo<'a, 'foo>(
                    arg1: &'a i32,  // Single letter lifetime = `$a` is possible
                    arg2: &'foo i32,  // Multi-character lifetime
                    arg3: &'foo i32,  // Same lifetime used for 2 places
                    arg4: &'static i32,
                    arg5: &'_ i32,
                    arg6: &'_ i32,
                ) -> &'foo i32 {
                    unimplemented!("args: {arg1}, {arg2}, {arg3}, {arg4}, {arg5}, {arg6}")
                }
            "#;
        test_format_item(test_src, "foo", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                  std::int32_t const& [[clang::annotate_type("lifetime", "foo")]]
                  foo(
                    std::int32_t const& [[clang::annotate_type("lifetime", "a")]] arg1,
                    std::int32_t const& [[clang::annotate_type("lifetime", "foo")]] arg2,
                    std::int32_t const& [[clang::annotate_type("lifetime", "foo")]] arg3,
                    std::int32_t const& [[clang::annotate_type("lifetime", "static")]] arg4,
                    std::int32_t const& [[clang::annotate_type("lifetime", "__anon1")]] arg5,
                    std::int32_t const& [[clang::annotate_type("lifetime", "__anon2")]] arg6);
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C"
                    std::int32_t const& [[clang::annotate_type("lifetime", "foo")]]
                    ...(
                        std::int32_t const& [[clang::annotate_type("lifetime", "a")]],
                        std::int32_t const& [[clang::annotate_type("lifetime", "foo")]],
                        std::int32_t const& [[clang::annotate_type("lifetime", "foo")]],
                        std::int32_t const& [[clang::annotate_type("lifetime", "static")]],
                        std::int32_t const& [[clang::annotate_type("lifetime", "__anon1")]],
                        std::int32_t const& [[clang::annotate_type("lifetime", "__anon2")]]);
                    }
                    inline
                    std::int32_t const& [[clang::annotate_type("lifetime", "foo")]]
                    foo(
                        std::int32_t const& [[clang::annotate_type("lifetime", "a")]] arg1,
                        std::int32_t const& [[clang::annotate_type("lifetime", "foo")]] arg2,
                        std::int32_t const& [[clang::annotate_type("lifetime", "foo")]] arg3,
                        std::int32_t const& [[clang::annotate_type("lifetime", "static")]] arg4,
                        std::int32_t const& [[clang::annotate_type("lifetime", "__anon1")]] arg5,
                        std::int32_t const& [[clang::annotate_type("lifetime", "__anon2")]] arg6) {
                      return __crubit_internal::...(arg1, arg2, arg3, arg4, arg5, arg6);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn ...<'a, 'foo, '__anon1, '__anon2>(
                        arg1: &'a i32,
                        arg2: &'foo i32,
                        arg3: &'foo i32,
                        arg4: &'static i32,
                        arg5: &'__anon1 i32,
                        arg6: &'__anon2 i32
                    ) -> &'foo i32 {
                        ::rust_out::foo(arg1, arg2, arg3, arg4, arg5, arg6)
                    }
                }
            );
        });
    }

    /// Test of lifetime-generic function with a `where` clause.
    ///
    /// The `where` constraint below is a bit silly (why not just use `'static`
    /// directly), but it seems prudent to test and confirm that we disable
    /// generation of bindings for generic functions with `where` clauses
    /// (because it is unclear if such constraints can be replicated
    /// in C++).
    #[test]
    fn test_format_item_lifetime_generic_fn_with_where_clause() {
        let test_src = r#"
                pub fn foo<'a>(arg: &'a i32) where 'a : 'static {
                    unimplemented!("{arg}")
                }
            "#;
        test_format_item(test_src, "foo", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Generic functions are not supported yet (b/259749023)");
        });
    }

    #[test]
    fn test_format_item_unsupported_type_generic_fn() {
        let test_src = r#"
                use std::fmt::Display;
                pub fn generic_function<T: Default + Display>() {
                    println!("{}", T::default());
                }
            "#;
        test_format_item(test_src, "generic_function", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Generic functions are not supported yet (b/259749023)");
        });
    }

    #[test]
    fn test_format_item_unsupported_type_generic_struct() {
        let test_src = r#"
                pub struct Point<T> {
                    pub x: T,
                    pub y: T,
                }
            "#;
        test_format_item(test_src, "Point", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Generic types are not supported yet (b/259749095)");
        });
    }

    #[test]
    fn test_format_item_unsupported_lifetime_generic_struct() {
        let test_src = r#"
                pub struct Point<'a> {
                    pub x: &'a i32,
                    pub y: &'a i32,
                }

                impl<'a> Point<'a> {
                    // Some lifetimes are bound at the `impl` / `struct` level (the lifetime is
                    // hidden underneath the `Self` type), and some at the `fn` level.
                    pub fn new<'b, 'c>(_x: &'b i32, _y: &'c i32) -> Self { unimplemented!() }
                }
            "#;
        test_format_item(test_src, "Point", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Generic types are not supported yet (b/259749095)");
        });
    }

    #[test]
    fn test_format_item_unsupported_generic_enum() {
        let test_src = r#"
                pub enum Point<T> {
                    Cartesian{x: T, y: T},
                    Polar{angle: T, dist: T},
                }
            "#;
        test_format_item(test_src, "Point", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Generic types are not supported yet (b/259749095)");
        });
    }

    #[test]
    fn test_format_item_unsupported_generic_union() {
        let test_src = r#"
                pub union SomeUnion<T> {
                    pub x: std::mem::ManuallyDrop<T>,
                    pub y: i32,
                }
            "#;
        test_format_item(test_src, "SomeUnion", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Generic types are not supported yet (b/259749095)");
        });
    }

    #[test]
    fn test_format_item_unsupported_fn_async() {
        let test_src = r#"
                pub async fn async_function() {}
            "#;
        test_format_item(test_src, "async_function", |result| {
            let err = result.unwrap_err();
            assert_eq!(
                err,
                "Error formatting function return type: \
                             The following Rust type is not supported yet: \
                             impl std::future::Future<Output = ()>"
            );
        });
    }

    #[test]
    fn test_format_item_fn_rust_abi() {
        let test_src = r#"
                pub fn add(x: f64, y: f64) -> f64 { x * y }
            "#;
        test_format_item(test_src, "add", |result| {
            // TODO(b/261074843): Re-add thunk name verification once we are using stable
            // name mangling (which may be coming in Q1 2023).  (This might mean
            // reverting cl/492333432 + manual review and tweaks.)
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    double add(double x, double y);
                }
            );
            assert!(result.cc_details.prereqs.is_empty());
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" double ...(double, double);
                    }
                    ...
                    inline double add(double x, double y) {
                        return __crubit_internal::...(x, y);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C"
                    fn ...(x: f64, y: f64) -> f64 {
                        ::rust_out::add(x, y)
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_rust_abi_with_param_taking_struct_by_value22() {
        let test_src = r#"
                use std::slice;
                pub struct S(i32);
                pub unsafe fn transmute_slice(
                    slice_ptr: *const u8,
                    slice_len: usize,
                    element_size: usize,
                    s: S,
                ) -> i32 {
                    let len_in_bytes = slice_len * element_size;
                    let b = slice::from_raw_parts(slice_ptr as *const u8, len_in_bytes);
                    if b.len() == len_in_bytes {
                        s.0
                    } else {
                        0
                    }
                }
            "#;
        test_format_item(test_src, "transmute_slice", |result| {
            let result = result.unwrap().unwrap();
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    unsafe extern "C"
                    fn ...(...) -> i32 {
                        unsafe {
                            ::rust_out::transmute_slice(..., ..., ..., s.assume_init_read() )
                        }
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_rust_abi_with_param_taking_struct_by_value() {
        let test_src = r#"
                pub struct S(i32);
                pub fn into_i32(s: S) -> i32 { s.0 }
            "#;
        test_format_item(test_src, "into_i32", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    std::int32_t into_i32(::rust_out::S s);
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" std::int32_t ...(::rust_out::S*);
                    }
                    ...
                    inline std::int32_t into_i32(::rust_out::S s) {
                        return __crubit_internal::...(&s);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C"
                    fn ...(s: &mut ::core::mem::MaybeUninit<::rust_out::S>) -> i32 {
                        ::rust_out::into_i32(unsafe { s.assume_init_read() })
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_rust_abi_returning_struct_by_value() {
        let test_src = r#"
                #![allow(dead_code)]

                pub struct S(i32);
                pub fn create(i: i32) -> S { S(i) }
            "#;
        test_format_item(test_src, "create", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ::rust_out::S create(std::int32_t i);
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" void ...(std::int32_t, ::rust_out::S* __ret_ptr);
                    }
                    ...
                    inline ::rust_out::S create(std::int32_t i) {
                        crubit::ReturnValueSlot<::rust_out::S> __ret_slot;
                        __crubit_internal::...(i, __ret_slot.Get());
                        return std::move(__ret_slot).AssumeInitAndTakeValue();
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C"
                    fn ...(
                        i: i32,
                        __ret_slot: &mut ::core::mem::MaybeUninit<::rust_out::S>
                    ) -> () {
                        __ret_slot.write(::rust_out::create(i));
                    }
                }
            );
        });
    }

    /// `test_format_item_fn_rust_abi` tests a function call that is not a
    /// C-ABI, and is not the default Rust ABI.  It can't use `"stdcall"`,
    /// because it is not supported on the targets where Crubit's tests run.
    /// So, it ended up using `"vectorcall"`.
    ///
    /// This test almost entirely replicates `test_format_item_fn_rust_abi`,
    /// except for the `extern "vectorcall"` part in the `test_src` test
    /// input.
    ///
    /// This test verifies the current behavior that gives reasonable and
    /// functional FFI bindings.  OTOH, in the future we may decide to avoid
    /// having the extra thunk for cases where the given non-C-ABI function
    /// call convention is supported by both C++ and Rust
    /// (see also `format_cc_call_conv_as_clang_attribute` in
    /// `rs_bindings_from_cc/src_code_gen.rs`)
    #[test]
    fn test_format_item_fn_vectorcall_abi() {
        let test_src = r#"
                #![feature(abi_vectorcall)]
                pub extern "vectorcall" fn add(x: f64, y: f64) -> f64 { x * y }
            "#;
        test_format_item(test_src, "add", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    double add(double x, double y);
                }
            );
            assert!(result.cc_details.prereqs.is_empty());
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" double ...(double, double);
                    }
                    ...
                    inline double add(double x, double y) {
                        return __crubit_internal::...(x, y);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C"
                    fn ...(x: f64, y: f64) -> f64 {
                        ::rust_out::add(x, y)
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_unsupported_fn_variadic() {
        let test_src = r#"
                #![feature(c_variadic)]

                #[no_mangle]
                pub unsafe extern "C" fn variadic_function(_fmt: *const u8, ...) {}
            "#;
        test_format_item(test_src, "variadic_function", |result| {
            // TODO(b/254097223): Add support for variadic functions.
            let err = result.unwrap_err();
            assert_eq!(err, "C variadic functions are not supported (b/254097223)");
        });
    }

    #[test]
    fn test_format_item_fn_params() {
        let test_src = r#"
                #[allow(unused_variables)]
                #[no_mangle]
                pub extern "C" fn foo(b: bool, f: f64) {}
            "#;
        test_format_item(test_src, "foo", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    extern "C" void foo(bool b, double f);
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_param_name_reserved_keyword() {
        let test_src = r#"
                #[allow(unused_variables)]
                #[no_mangle]
                pub extern "C" fn some_function(reinterpret_cast: f64) {}
            "#;
        test_format_item(test_src, "some_function", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    extern "C" void some_function(double __param_0);
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_with_multiple_anonymous_parameter_names() {
        let test_src = r#"
                pub fn foo(_: f64, _: f64) {}
            "#;
        test_format_item(test_src, "foo", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    void foo(double __param_0, double __param_1);
                }
            );
            assert!(result.cc_details.prereqs.is_empty());
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" void ...(double, double);
                    }
                    ...
                    inline void foo(double __param_0, double __param_1) {
                        return __crubit_internal::...(__param_0, __param_1);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn ...(__param_0: f64, __param_1: f64) -> () {
                        ::rust_out::foo(__param_0, __param_1)
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_fn_with_destructuring_parameter_name() {
        let test_src = r#"
                pub struct S {
                    pub f1: i32,
                    pub f2: i32,
                }

                // This test mostly focuses on the weird parameter "name" below.
                // See also
                // https://doc.rust-lang.org/reference/items/functions.html#function-parameters
                // which points out that function parameters are just irrefutable patterns.
                pub fn func(S{f1, f2}: S) -> i32 { f1 + f2 }
            "#;
        test_format_item(test_src, "func", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    std::int32_t func(::rust_out::S __param_0);
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" std::int32_t ...(::rust_out::S*);
                    }
                    ...
                    inline std::int32_t func(::rust_out::S __param_0) {
                        return __crubit_internal::...(&__param_0);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn ...(
                        __param_0: &mut ::core::mem::MaybeUninit<::rust_out::S>
                    ) -> i32 {
                        ::rust_out::func(unsafe {__param_0.assume_init_read() })
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_unsupported_fn_param_type() {
        let test_src = r#"
                pub fn foo(_param: (i32, i32)) {}
            "#;
        test_format_item(test_src, "foo", |result| {
            let err = result.unwrap_err();
            assert_eq!(
                err,
                "Error handling parameter #0: \
                             Tuples are not supported yet: (i32, i32) (b/254099023)"
            );
        });
    }

    #[test]
    fn test_format_item_unsupported_fn_param_type_unit() {
        let test_src = r#"
                #[no_mangle]
                pub fn fn_with_params(_param: ()) {}
            "#;
        test_format_item(test_src, "fn_with_params", |result| {
            let err = result.unwrap_err();
            assert_eq!(
                err,
                "Error handling parameter #0: \
                             `()` / `void` is only supported as a return type (b/254507801)"
            );
        });
    }

    #[test]
    fn test_format_item_unsupported_fn_param_type_never() {
        let test_src = r#"
                #![feature(never_type)]

                #[no_mangle]
                pub extern "C" fn fn_with_params(_param: !) {}
            "#;
        test_format_item(test_src, "fn_with_params", |result| {
            let err = result.unwrap_err();
            assert_eq!(
                err,
                "Error handling parameter #0: \
                 The never type `!` is only supported as a return type (b/254507801)"
            );
        });
    }

    /// This is a test for a regular struct - a struct with named fields.
    /// https://doc.rust-lang.org/reference/items/structs.html refers to this kind of struct as
    /// `StructStruct` or "nominal struct type".
    #[test]
    fn test_format_item_struct_with_fields() {
        let test_src = r#"
                pub struct SomeStruct {
                    pub x: i32,
                    pub y: i32,
                }

                const _: () = assert!(std::mem::size_of::<SomeStruct>() == 8);
                const _: () = assert!(std::mem::align_of::<SomeStruct>() == 4);
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] SomeStruct final {
                        public:
                            __COMMENT__ "`SomeStruct` doesn't implement the `Default` trait"
                            SomeStruct() = delete;

                            __COMMENT__ "No custom `Drop` impl and no custom \"drop glue\" required"
                            ~SomeStruct() = default;
                            SomeStruct(SomeStruct&&) = default;
                            SomeStruct& operator=(SomeStruct&&) = default;

                            __COMMENT__ "`SomeStruct` doesn't implement the `Clone` trait"
                            SomeStruct(const SomeStruct&) = delete;
                            SomeStruct& operator=(const SomeStruct&) = delete;
                        public: union { ... std::int32_t x; };
                        public: union { ... std::int32_t y; };
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeStruct) == 8, ...);
                    static_assert(alignof(SomeStruct) == 4, ...);
                    static_assert(std::is_trivially_destructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_constructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_assignable_v<SomeStruct>);
                    inline void SomeStruct::__crubit_field_offset_assertions() {
                      static_assert(0 == offsetof(SomeStruct, x));
                      static_assert(4 == offsetof(SomeStruct, y));
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeStruct>() == 8);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeStruct>() == 4);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, x) == 0);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, y) == 4);
                }
            );
        });
    }

    /// This is a test for `TupleStruct` or "tuple struct" - for more details
    /// please refer to https://doc.rust-lang.org/reference/items/structs.html
    #[test]
    fn test_format_item_struct_with_tuple() {
        let test_src = r#"
                pub struct TupleStruct(pub i32, pub i32);
                const _: () = assert!(std::mem::size_of::<TupleStruct>() == 8);
                const _: () = assert!(std::mem::align_of::<TupleStruct>() == 4);
            "#;
        test_format_item(test_src, "TupleStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] TupleStruct final {
                        public:
                            __COMMENT__ "`TupleStruct` doesn't implement the `Default` trait"
                            TupleStruct() = delete;

                            __COMMENT__ "No custom `Drop` impl and no custom \"drop glue\" required"
                            ~TupleStruct() = default;
                            TupleStruct(TupleStruct&&) = default;
                            TupleStruct& operator=(TupleStruct&&) = default;

                            __COMMENT__ "`TupleStruct` doesn't implement the `Clone` trait"
                            TupleStruct(const TupleStruct&) = delete;
                            TupleStruct& operator=(const TupleStruct&) = delete;
                        public: union { ... std::int32_t __field0; };
                        public: union { ... std::int32_t __field1; };
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(TupleStruct) == 8, ...);
                    static_assert(alignof(TupleStruct) == 4, ...);
                    static_assert(std::is_trivially_destructible_v<TupleStruct>);
                    static_assert(std::is_trivially_move_constructible_v<TupleStruct>);
                    static_assert(std::is_trivially_move_assignable_v<TupleStruct>);
                    inline void TupleStruct::__crubit_field_offset_assertions() {
                      static_assert(0 == offsetof(TupleStruct, __field0));
                      static_assert(4 == offsetof(TupleStruct, __field1));
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::TupleStruct>() == 8);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::TupleStruct>() == 4);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::TupleStruct, 0) == 0);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::TupleStruct, 1) == 4);
                }
            );
        });
    }

    /// This test the scenario where Rust lays out field in a different order
    /// than the source order.
    #[test]
    fn test_format_item_struct_with_reordered_field_offsets() {
        let test_src = r#"
                pub struct SomeStruct {
                    pub field1: i16,
                    pub field2: i32,
                    pub field3: i16,
                }

                const _: () = assert!(std::mem::size_of::<SomeStruct>() == 8);
                const _: () = assert!(std::mem::align_of::<SomeStruct>() == 4);
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] SomeStruct final {
                        ...
                        // The particular order below is not guaranteed,
                        // so we may need to adjust this test assertion
                        // (if Rust changes how it lays out the fields).
                        public: union { ... std::int32_t field2; };
                        public: union { ... std::int16_t field1; };
                        public: union { ... std::int16_t field3; };
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeStruct) == 8, ...);
                    static_assert(alignof(SomeStruct) == 4, ...);
                    static_assert(std::is_trivially_destructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_constructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_assignable_v<SomeStruct>);
                    inline void SomeStruct::__crubit_field_offset_assertions() {
                      static_assert(0 == offsetof(SomeStruct, field2));
                      static_assert(4 == offsetof(SomeStruct, field1));
                      static_assert(6 == offsetof(SomeStruct, field3));
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeStruct>() == 8);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeStruct>() == 4);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, field2)
                                           == 0);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, field1)
                                           == 4);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, field3)
                                           == 6);
                }
            );
        });
    }

    #[test]
    fn test_format_item_struct_with_packed_layout() {
        let test_src = r#"
                #[repr(packed(1))]
                pub struct SomeStruct {
                    pub field1: u16,
                    pub field2: u32,
                }
                const _: () = assert!(::std::mem::size_of::<SomeStruct>() == 6);
                const _: () = assert!(::std::mem::align_of::<SomeStruct>() == 1);
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(1) [[clang::trivial_abi]] __attribute__((packed)) SomeStruct final {
                        ...
                        public: union { ... std::uint16_t field1; };
                        public: union { ... std::uint32_t field2; };
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeStruct) == 6, ...);
                    static_assert(alignof(SomeStruct) == 1, ...);
                    static_assert(std::is_trivially_destructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_constructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_assignable_v<SomeStruct>);
                    inline void SomeStruct::__crubit_field_offset_assertions() {
                      static_assert(0 == offsetof(SomeStruct, field1));
                      static_assert(2 == offsetof(SomeStruct, field2));
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeStruct>() == 6);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeStruct>() == 1);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, field1)
                                           == 0);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, field2)
                                           == 2);
                }
            );
        });
    }

    #[test]
    fn test_format_item_struct_with_explicit_padding_in_generated_code() {
        let test_src = r#"
                pub struct SomeStruct {
                    pub f1: u8,
                    pub f2: u32,
                }
                const _: () = assert!(::std::mem::size_of::<SomeStruct>() == 8);
                const _: () = assert!(::std::mem::align_of::<SomeStruct>() == 4);
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] SomeStruct final {
                        ...
                        public: union { ... std::uint32_t f2; };
                        public: union { ... std::uint8_t f1; };
                        private: unsigned char __padding0[3];
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeStruct) == 8, ...);
                    static_assert(alignof(SomeStruct) == 4, ...);
                    static_assert(std::is_trivially_destructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_constructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_assignable_v<SomeStruct>);
                    inline void SomeStruct::__crubit_field_offset_assertions() {
                      static_assert(0 == offsetof(SomeStruct, f2));
                      static_assert(4 == offsetof(SomeStruct, f1));
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeStruct>() == 8);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeStruct>() == 4);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, f2) == 0);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, f1) == 4);
                }
            );
        });
    }

    #[test]
    fn test_format_item_static_method() {
        let test_src = r#"
                #![allow(dead_code)]

                /// No-op `f32` placeholder is used, because ZSTs are not supported
                /// (b/258259459).
                pub struct Math(f32);

                impl Math {
                    pub fn add_i32(x: f32, y: f32) -> f32 {
                        x + y
                    }
                }
            "#;
        test_format_item(test_src, "Math", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... Math final {
                        ...
                        public:
                          ...
                          static float add_i32(float x, float y);
                        ...
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" float ... (float, float);
                    }
                    inline float Math::add_i32(float x, float y) {
                      return __crubit_internal::...(x, y);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn ...(x: f32, y: f32) -> f32 {
                        ::rust_out::Math::add_i32(x, y)
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_static_method_with_generic_type_parameters() {
        let test_src = r#"
                #![allow(dead_code)]

                /// No-op `f32` placeholder is used, because ZSTs are not supported
                /// (b/258259459).
                pub struct SomeStruct(f32);

                impl SomeStruct {
                    // To make this testcase distinct / non-overlapping wrt
                    // test_format_item_static_method_with_generic_lifetime_parameters
                    // `t` is taken by value below.
                    pub fn generic_method<T: Clone>(t: T) -> T {
                        t.clone()
                    }
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let unsupported_msg = "Error generating bindings for `SomeStruct::generic_method` \
                                   defined at <crubit_unittests.rs>;l=12: \
                                   Generic functions are not supported yet (b/259749023)";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        __COMMENT__ #unsupported_msg
                        ...
                    };
                    ...
                }
            );
            assert_cc_not_matches!(result.cc_details.tokens, quote! { SomeStruct::generic_method },);
            assert_rs_not_matches!(result.rs_details, quote! { generic_method },);
        });
    }

    #[test]
    fn test_format_item_static_method_with_generic_lifetime_parameters_at_fn_level() {
        let test_src = r#"
                #![allow(dead_code)]

                /// No-op `f32` placeholder is used, because ZSTs are not supported
                /// (b/258259459).
                pub struct SomeStruct(f32);

                impl SomeStruct {
                    pub fn fn_taking_reference<'a>(x: &'a i32) -> i32 { *x }
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        static std::int32_t fn_taking_reference(
                            std::int32_t const& [[clang::annotate_type("lifetime", "a")]] x);
                        ...
                    };
                    ...
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C" std::int32_t ...(
                        std::int32_t const& [[clang::annotate_type("lifetime", "a")]]);
                    }
                    inline std::int32_t SomeStruct::fn_taking_reference(
                        std::int32_t const& [[clang::annotate_type("lifetime", "a")]] x) {
                      return __crubit_internal::...(x);
                    }
                },
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn ...<'a>(x: &'a i32) -> i32 {
                        ::rust_out::SomeStruct::fn_taking_reference(x)
                    }
                },
            );
        });
    }

    #[test]
    fn test_format_item_static_method_with_generic_lifetime_parameters_at_impl_level() {
        let test_src = r#"
                #![allow(dead_code)]

                /// No-op `f32` placeholder is used, because ZSTs are not supported
                /// (b/258259459).
                pub struct SomeStruct(f32);

                impl<'a> SomeStruct {
                    pub fn fn_taking_reference(x: &'a i32) -> i32 { *x }
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let unsupported_msg = "Error generating bindings for `SomeStruct::fn_taking_reference` \
                                   defined at <crubit_unittests.rs>;l=9: \
                                   Generic functions are not supported yet (b/259749023)";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        __COMMENT__ #unsupported_msg
                        ...
                    };
                    ...
                }
            );
            assert_cc_not_matches!(
                result.cc_details.tokens,
                quote! { SomeStruct::fn_taking_reference },
            );
            assert_rs_not_matches!(result.rs_details, quote! { fn_taking_reference },);
        });
    }

    fn test_format_item_method_taking_self_by_value(test_src: &str) {
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        float into_f32() &&;
                        ...
                    };
                    ...
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C" float ...(::rust_out::SomeStruct*);
                    }
                    inline float SomeStruct::into_f32() && {
                      return __crubit_internal::...(this);
                    }
                },
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    ...
                    #[no_mangle]
                    extern "C" fn ...(__self: &mut ::core::mem::MaybeUninit<::rust_out::SomeStruct>) -> f32 {
                        ::rust_out::SomeStruct::into_f32(unsafe { __self.assume_init_read() })
                    }
                    ...
                },
            );
        });
    }

    #[test]
    fn test_format_item_method_taking_self_by_value_implicit_type() {
        let test_src = r#"
                pub struct SomeStruct(pub f32);

                impl SomeStruct {
                    pub fn into_f32(self) -> f32 {
                        self.0
                    }
                }
            "#;
        test_format_item_method_taking_self_by_value(test_src);
    }

    /// One difference from
    /// `test_format_item_method_taking_self_by_value_implicit_type` is that
    /// `fn_sig.decl.implicit_self` is `ImplicitSelfKind::None` here (vs
    /// `ImplicitSelfKind::Imm` in the other test).
    #[test]
    fn test_format_item_method_taking_self_by_value_explicit_type() {
        let test_src = r#"
                pub struct SomeStruct(pub f32);

                impl SomeStruct {
                    pub fn into_f32(self: SomeStruct) -> f32 {
                        self.0
                    }
                }
            "#;
        test_format_item_method_taking_self_by_value(test_src);
    }

    fn test_format_item_method_taking_self_by_const_ref(test_src: &str) {
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        float get_f32() const [[clang::annotate_type("lifetime", "__anon1")]];
                        ...
                    };
                    ...
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C" float ...(
                        ::rust_out::SomeStruct const& [[clang::annotate_type("lifetime",
                                                                             "__anon1")]]);
                    }
                    inline float SomeStruct::get_f32()
                        const [[clang::annotate_type("lifetime", "__anon1")]] {
                      return __crubit_internal::...(*this);
                    }
                },
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn ...<'__anon1>(__self: &'__anon1 ::rust_out::SomeStruct) -> f32 {
                        ::rust_out::SomeStruct::get_f32(__self)
                    }
                    ...
                },
            );
        });
    }

    #[test]
    fn test_format_item_method_taking_self_by_const_ref_implicit_type() {
        let test_src = r#"
                pub struct SomeStruct(pub f32);

                impl SomeStruct {
                    pub fn get_f32(&self) -> f32 {
                        self.0
                    }
                }
            "#;
        test_format_item_method_taking_self_by_const_ref(test_src);
    }

    #[test]
    fn test_format_item_method_taking_self_by_const_ref_explicit_type() {
        let test_src = r#"
                pub struct SomeStruct(pub f32);

                impl SomeStruct {
                    pub fn get_f32(self: &SomeStruct) -> f32 {
                        self.0
                    }
                }
            "#;
        test_format_item_method_taking_self_by_const_ref(test_src);
    }

    fn test_format_item_method_taking_self_by_mutable_ref(test_src: &str) {
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        void set_f32(float new_value)
                            [[clang::annotate_type("lifetime", "__anon1")]];
                        ...
                    };
                    ...
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C" void ...(
                        ::rust_out::SomeStruct& [[clang::annotate_type("lifetime", "__anon1")]],
                        float);
                    }
                    inline void SomeStruct::set_f32(float new_value)
                            [[clang::annotate_type("lifetime", "__anon1")]] {
                      return __crubit_internal::...(*this, new_value);
                    }
                },
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn ...<'__anon1>(
                        __self: &'__anon1 mut ::rust_out::SomeStruct,
                        new_value: f32
                    ) -> () {
                        ::rust_out::SomeStruct::set_f32(__self, new_value)
                    }
                    ...
                },
            );
        });
    }

    #[test]
    fn test_format_item_method_taking_self_by_mutable_ref_implicit_type() {
        let test_src = r#"
                pub struct SomeStruct(pub f32);

                impl SomeStruct {
                    pub fn set_f32(&mut self, new_value: f32) {
                        self.0 = new_value;
                    }
                }
            "#;
        test_format_item_method_taking_self_by_mutable_ref(test_src);
    }

    #[test]
    fn test_format_item_method_taking_self_by_mutable_ref_explicit_type() {
        let test_src = r#"
                pub struct SomeStruct(pub f32);

                impl SomeStruct {
                    pub fn set_f32(self: &mut SomeStruct, new_value: f32) {
                        self.0 = new_value;
                    }
                }
            "#;
        test_format_item_method_taking_self_by_mutable_ref(test_src);
    }

    #[test]
    fn test_format_item_method_taking_self_by_arc() {
        let test_src = r#"
                use std::sync::Arc;

                pub struct SomeStruct(pub f32);

                impl SomeStruct {
                    pub fn get_f32(self: Arc<Self>) -> f32 {
                        self.0
                    }
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let unsupported_msg = "Error generating bindings for `SomeStruct::get_f32` \
                                   defined at <crubit_unittests.rs>;l=7: \
                                   Error handling parameter #0: \
                                   Generic types are not supported yet (b/259749095)";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        __COMMENT__ #unsupported_msg
                        ...
                    };
                    ...
                }
            );
            assert_cc_not_matches!(result.cc_details.tokens, quote! { SomeStruct::get_f32 },);
            assert_rs_not_matches!(result.rs_details, quote! { get_f32 },);
        });
    }

    #[test]
    fn test_format_item_method_taking_self_by_pinned_mut_ref() {
        let test_src = r#"
                use core::pin::Pin;

                pub struct SomeStruct(f32);

                impl SomeStruct {
                    pub fn set_f32(mut self: Pin<&mut Self>, f: f32) {
                        self.0 = f;
                    }
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let unsupported_msg = "Error generating bindings for `SomeStruct::set_f32` \
                                   defined at <crubit_unittests.rs>;l=7: \
                                   Error handling parameter #0: \
                                   Generic types are not supported yet (b/259749095)";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        __COMMENT__ #unsupported_msg
                        ...
                    };
                    ...
                }
            );
            assert_cc_not_matches!(result.cc_details.tokens, quote! { SomeStruct::set_f32 },);
            assert_rs_not_matches!(result.rs_details, quote! { set_f32 },);
        });
    }

    #[test]
    fn test_format_item_struct_with_default_constructor() {
        let test_src = r#"
                #![allow(dead_code)]

                #[derive(Default)]
                pub struct Point(i32, i32);
            "#;
        test_format_item(test_src, "Point", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... Point final {
                        ...
                        public:
                          __COMMENT__ "Default::default"
                          Point();
                        ...
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                        extern "C" void ...(::rust_out::Point* __ret_ptr);
                    }
                    inline Point::Point() {
                        ...(this);
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                   #[no_mangle]
                   extern "C" fn ...(
                       __ret_slot: &mut ::core::mem::MaybeUninit<::rust_out::Point>
                   ) -> () {
                       __ret_slot.write(<::rust_out::Point as ::core::default::Default>::default());
                   }
                }
            );
        });
    }

    #[test]
    fn test_format_item_struct_with_copy_trait() {
        let test_src = r#"
                #![allow(dead_code)]

                #[derive(Clone, Copy)]
                pub struct Point(i32, i32);
            "#;
        let msg = "Rust types that are `Copy` get trivial, `default` C++ copy constructor \
                   and assignment operator.";
        test_format_item(test_src, "Point", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... Point final {
                        ...
                        public:
                          ...
                          __COMMENT__ #msg
                          Point(const Point&) = default;
                          Point& operator=(const Point&) = default;
                          ...
                    };
                }
            );

            // Trivial copy doesn't require any C++ details except `static_assert`s.
            assert_cc_not_matches!(result.cc_details.tokens, quote! { Point::Point(const Point&) },);
            assert_cc_not_matches!(
                result.cc_details.tokens,
                quote! { Point::operator=(const Point&) },
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(std::is_trivially_copy_constructible_v<Point>);
                    static_assert(std::is_trivially_copy_assignable_v<Point>);
                },
            );

            // Trivial copy doesn't require any Rust details.
            assert_rs_not_matches!(result.rs_details, quote! { Copy });
            assert_rs_not_matches!(result.rs_details, quote! { copy });
        });
    }

    /// Test of `format_copy_ctor_and_assignment_operator` when the ADT
    /// implements a `Clone` trait.
    ///
    /// Notes:
    /// * `Copy` trait is covered in `test_format_item_struct_with_copy_trait`.
    /// * The test below implements `clone` and uses the default `clone_from`.
    #[test]
    fn test_format_item_struct_with_clone_trait() {
        let test_src = r#"
                #![allow(dead_code)]

                pub struct Point(i32, i32);
                impl Clone for Point {
                    fn clone(&self) -> Self {
                        unimplemented!()
                    }
                }
            "#;
        test_format_item(test_src, "Point", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... Point final {
                        ...
                        public:
                          ...
                          __COMMENT__ "Clone::clone"
                          Point(const Point&);

                          __COMMENT__ "Clone::clone_from"
                          Point& operator=(const Point&);
                        ...
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C" void ...(
                        ::rust_out::Point const& [[clang::annotate_type("lifetime",
                                                                        "__anon1")]],
                        ::rust_out::Point* __ret_ptr);
                    }
                    namespace __crubit_internal {
                    extern "C" void ...(
                        ::rust_out::Point& [[clang::annotate_type("lifetime", "__anon1")]],
                        ::rust_out::Point const& [[clang::annotate_type("lifetime",
                                                                        "__anon2")]]);
                    }
                    inline Point::Point(const Point& other) {
                      __crubit_internal::...(other, this);
                    }
                    inline Point& Point::operator=(const Point& other) {
                      if (this != &other) {
                        __crubit_internal::...(*this, other);
                      }
                      return *this;
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    #[no_mangle]
                    extern "C" fn ...<'__anon1>(
                        __self: &'__anon1 ::rust_out::Point,
                        __ret_slot: &mut ::core::mem::MaybeUninit<::rust_out::Point>
                    ) -> () {
                        __ret_slot.write(
                            <::rust_out::Point as ::core::clone::Clone>::clone(__self)
                        );
                    }
                    #[no_mangle]
                    extern "C" fn ...<'__anon1, '__anon2>(
                        __self: &'__anon1 mut ::rust_out::Point,
                        source: &'__anon2 ::rust_out::Point
                    ) -> () {
                        <::rust_out::Point as ::core::clone::Clone>::clone_from(__self, source)
                    }
                }
            );
        });
    }

    #[test]
    fn test_format_item_unsupported_struct_with_name_that_is_reserved_keyword() {
        let test_src = r#"
                #[allow(non_camel_case_types)]
                pub struct reinterpret_cast {
                    pub x: i32,
                    pub y: i32,
                }
            "#;
        test_format_item(test_src, "reinterpret_cast", |result| {
            let err = result.unwrap_err();
            assert_eq!(
                err,
                "Error formatting item name: \
                             `reinterpret_cast` is a C++ reserved keyword \
                             and can't be used as a C++ identifier"
            );
        });
    }

    #[test]
    fn test_format_item_struct_with_unsupported_field_type() {
        let test_src = r#"
                pub struct SomeStruct {
                    pub successful_field: i32,
                    pub unsupported_field: Option<[i32; 3]>,
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let broken_field_msg = "Field type has been replaced with a blob of bytes: \
                                    Generic types are not supported yet (b/259749095)";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        private:
                            __COMMENT__ #broken_field_msg
                            unsigned char unsupported_field[16];
                        public:
                            union { ... std::int32_t successful_field; };
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                    ...
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeStruct) == 20, ...);
                    static_assert(alignof(SomeStruct) == 4, ...);
                    static_assert(std::is_trivially_destructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_constructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_assignable_v<SomeStruct>);
                    inline void SomeStruct::__crubit_field_offset_assertions() {
                      static_assert(0 == offsetof(SomeStruct, unsupported_field));
                      static_assert(16 == offsetof(SomeStruct, successful_field));
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeStruct>() == 20);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeStruct>() == 4);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct,
                                                                 unsupported_field) == 0);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct,
                                                                 successful_field) == 16);
                }
            );
        });
    }

    /// This test verifies how reference type fields are represented in the
    /// generated bindings.  See b/286256327.
    ///
    /// In some of the past discussions we tentatively decided that the
    /// generated bindings shouldn't use C++ references in fields - instead
    /// a C++ pointer should be used.  One reason is that C++ references
    /// cannot be assigned to (i.e. rebound), and therefore C++ pointers
    /// more accurately represent the semantics of Rust fields.  The pointer
    /// type should probably use some form of C++ annotations to mark it as
    /// non-nullable.
    #[test]
    fn test_format_item_struct_with_unsupported_field_of_reference_type() {
        let test_src = r#"
                // `'static` lifetime can be used in a non-generic struct - this let's us
                // test reference fieles without requiring support for generic structs.
                pub struct NonGenericSomeStruct {
                    pub reference_field: &'static i32,
                }
            "#;
        test_format_item(test_src, "NonGenericSomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let broken_field_msg = "Field type has been replaced with a blob of bytes: \
                                    Can't format `&'static i32`, because references \
                                    are only supported in function parameter types and \
                                    return types (b/286256327)";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    private:
                        __COMMENT__ #broken_field_msg
                        unsigned char reference_field[8];
                    ...
                }
            );
        });
    }

    /// This test verifies that `format_trait_thunks(..., drop_trait_id,
    /// ...).expect(...)` won't panic - the `format_adt_core` needs to
    /// verify that formatting of the fully qualified C++ name of the struct
    /// works fine.
    #[test]
    fn test_format_item_unsupported_struct_with_custom_drop_impl_in_reserved_name_module() {
        let test_src = r#"
                // This mimics the name of a public module used by
                // `icu_locid` in `extensions/mod.rs`.
                pub mod private {
                    #[derive(Default)]
                    pub struct SomeStruct {
                        pub x: i32,
                        pub y: i32,
                    }

                    impl Drop for SomeStruct {
                        fn drop(&mut self) {}
                    }
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let err = result.unwrap_err();
            assert_eq!(
                err,
                "Error formatting the fully-qualified C++ name of `SomeStruct: \
                 `private` is a C++ reserved keyword and can't be used as a C++ identifier",
            );
        });
    }

    fn test_format_item_struct_with_custom_drop_and_no_default_nor_clone_impl(
        test_src: &str,
        pass_by_value_line_number: i32,
    ) {
        test_format_item(test_src, "TypeUnderTest", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let move_deleted_msg = "C++ moves are deleted \
                                    because there's no non-destructive implementation available.";
            let pass_by_value_msg = format!(
                "Error generating bindings for `TypeUnderTest::pass_by_value` \
                        defined at <crubit_unittests.rs>;l={pass_by_value_line_number}: \
                 Can't pass the return type by value without a move constructor"
            );
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... TypeUnderTest final {
                        ...
                        public:
                          ...
                          __COMMENT__ "Drop::drop"
                          ~TypeUnderTest();

                          __COMMENT__ #move_deleted_msg
                          TypeUnderTest(TypeUnderTest&&) = delete;
                          TypeUnderTest& operator=(TypeUnderTest&&) = delete;
                          ...
                          __COMMENT__ #pass_by_value_msg
                          ...
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C" void ...(  // `drop` thunk decl
                        ::rust_out::TypeUnderTest& [[clang::annotate_type(
                            "lifetime", "__anon1")]]);
                    }
                    inline TypeUnderTest::~TypeUnderTest() {
                      __crubit_internal::...(*this);
                    }
                }
            );
            assert_cc_not_matches!(result.cc_details.tokens, quote! { pass_by_value });
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    ...
                    #[no_mangle]
                    extern "C" fn ...(
                        __self: &mut ::core::mem::MaybeUninit<::rust_out::TypeUnderTest>
                    ) {
                        unsafe { __self.assume_init_drop() };
                    }
                    ...
                }
            );
            assert_rs_not_matches!(result.rs_details, quote! { pass_by_value });
        });
    }

    #[test]
    fn test_format_item_struct_with_custom_drop_impl_and_no_default_nor_clone_impl() {
        let test_src = r#"
                pub struct TypeUnderTest {
                    pub x: i32,
                    pub y: i32,
                }

                impl Drop for TypeUnderTest {
                    fn drop(&mut self) {}
                }

                impl TypeUnderTest {
                    pub fn pass_by_value() -> Self { unimplemented!() }
                }
            "#;
        let pass_by_value_line_number = 12;
        test_format_item_struct_with_custom_drop_and_no_default_nor_clone_impl(
            test_src,
            pass_by_value_line_number,
        );
    }

    #[test]
    fn test_format_item_struct_with_custom_drop_glue_and_no_default_nor_clone_impl() {
        let test_src = r#"
                #![allow(dead_code)]

                // `i32` is present to avoid hitting the ZST checks related to (b/258259459)
                struct StructWithCustomDropImpl(i32);

                impl Drop for StructWithCustomDropImpl {
                    fn drop(&mut self) {
                        println!("dropping!");
                    }
                }

                pub struct TypeUnderTest {
                    field: StructWithCustomDropImpl,
                }

                impl TypeUnderTest {
                    pub fn pass_by_value() -> Self { unimplemented!() }
                }
            "#;
        let pass_by_value_line_number = 18;
        test_format_item_struct_with_custom_drop_and_no_default_nor_clone_impl(
            test_src,
            pass_by_value_line_number,
        );
    }

    fn test_format_item_struct_with_custom_drop_and_with_default_impl(test_src: &str) {
        test_format_item(test_src, "TypeUnderTest", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... TypeUnderTest final {
                        ...
                        public:
                          ...
                          __COMMENT__ "Drop::drop"
                          ~TypeUnderTest();
                          TypeUnderTest(TypeUnderTest&&);
                          TypeUnderTest& operator=(
                              TypeUnderTest&&);
                          ...
                          static ::rust_out::TypeUnderTest pass_by_value();
                          ...
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C" void ...(  // `drop` thunk decl
                        ::rust_out::TypeUnderTest& [[clang::annotate_type(
                            "lifetime", "__anon1")]]);
                    }
                    inline TypeUnderTest::~TypeUnderTest() {
                      __crubit_internal::...(*this);
                    }
                    inline TypeUnderTest::TypeUnderTest(
                        TypeUnderTest&& other)
                        : TypeUnderTest() {
                      *this = std::move(other);
                    }
                    inline TypeUnderTest& TypeUnderTest::operator=(
                        TypeUnderTest&& other) {
                      crubit::MemSwap(*this, other);
                      return *this;
                    }
                    namespace __crubit_internal {  // `pass_by_value` thunk decl
                    extern "C" void ...(::rust_out::TypeUnderTest* __ret_ptr);
                    }
                    inline ::rust_out::TypeUnderTest TypeUnderTest::pass_by_value() {
                      crubit::ReturnValueSlot<::rust_out::TypeUnderTest> __ret_slot;
                      __crubit_internal::...(__ret_slot.Get());
                      return std::move(__ret_slot).AssumeInitAndTakeValue();
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    ...
                    #[no_mangle]
                    extern "C" fn ...(
                        __self: &mut ::core::mem::MaybeUninit<::rust_out::TypeUnderTest>
                    ) {
                        unsafe { __self.assume_init_drop() };
                    }
                    #[no_mangle]
                    extern "C" fn ...(
                        __ret_slot: &mut ::core::mem::MaybeUninit<::rust_out::TypeUnderTest>
                    ) -> () {
                        __ret_slot.write(::rust_out::TypeUnderTest::pass_by_value());
                    }
                    ...
                }
            );
        });
    }

    #[test]
    fn test_format_item_struct_with_custom_drop_impl_and_with_default_impl() {
        let test_src = r#"
                #[derive(Default)]
                pub struct TypeUnderTest {
                    pub x: i32,
                    pub y: i32,
                }

                impl Drop for TypeUnderTest {
                    fn drop(&mut self) {}
                }

                impl TypeUnderTest {
                    pub fn pass_by_value() -> Self { unimplemented!() }
                }
            "#;
        test_format_item_struct_with_custom_drop_and_with_default_impl(test_src);
    }

    #[test]
    fn test_format_item_struct_with_custom_drop_glue_and_with_default_impl() {
        let test_src = r#"
                #![allow(dead_code)]

                // `i32` is present to avoid hitting the ZST checks related to (b/258259459)
                #[derive(Default)]
                struct StructWithCustomDropImpl(i32);

                impl Drop for StructWithCustomDropImpl {
                    fn drop(&mut self) {
                        println!("dropping!");
                    }
                }

                #[derive(Default)]
                pub struct TypeUnderTest {
                    field: StructWithCustomDropImpl,
                }

                impl TypeUnderTest {
                    pub fn pass_by_value() -> Self { unimplemented!() }
                }
            "#;
        test_format_item_struct_with_custom_drop_and_with_default_impl(test_src);
    }

    fn test_format_item_struct_with_custom_drop_and_no_default_and_clone(test_src: &str) {
        test_format_item(test_src, "TypeUnderTest", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... TypeUnderTest final {
                        ...
                        public:
                          ...
                          __COMMENT__ "Drop::drop"
                          ~TypeUnderTest();
                          ...
                          static ::rust_out::TypeUnderTest pass_by_value();
                          ...
                    };
                }
            );

            // Implicit, but not `=default`-ed move constructor and move assignment
            // operator.
            assert_cc_not_matches!(main_api.tokens, quote! { TypeUnderTest(TypeUnderTest&&) });
            assert_cc_not_matches!(main_api.tokens, quote! { operator=(TypeUnderTest&&) });
            // No definition of a custom move constructor nor move assignment operator.
            assert_cc_not_matches!(
                result.cc_details.tokens,
                quote! { TypeUnderTest(TypeUnderTest&&) },
            );
            assert_cc_not_matches!(result.cc_details.tokens, quote! { operator=(TypeUnderTest&&) },);

            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    namespace __crubit_internal {
                    extern "C" void ...(  // `drop` thunk decl
                        ::rust_out::TypeUnderTest& [[clang::annotate_type(
                            "lifetime", "__anon1")]]);
                    }
                    ...
                    namespace __crubit_internal {  // `pass_by_value` thunk decl
                    extern "C" void ...(::rust_out::TypeUnderTest* __ret_ptr);
                    }
                    inline ::rust_out::TypeUnderTest TypeUnderTest::pass_by_value() {
                      crubit::ReturnValueSlot<::rust_out::TypeUnderTest> __ret_slot;
                      __crubit_internal::...(__ret_slot.Get());
                      return std::move(__ret_slot).AssumeInitAndTakeValue();
                    }
                    ...
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    ...
                    #[no_mangle]
                    extern "C" fn ...(
                        __self: &mut ::core::mem::MaybeUninit<::rust_out::TypeUnderTest>
                    ) {
                        unsafe { __self.assume_init_drop() };
                    }
                    #[no_mangle]
                    extern "C" fn ...(
                        __ret_slot: &mut ::core::mem::MaybeUninit<::rust_out::TypeUnderTest>
                    ) -> () {
                        __ret_slot.write(::rust_out::TypeUnderTest::pass_by_value());
                    }
                    ...
                }
            );
        });
    }

    #[test]
    fn test_format_item_struct_with_custom_drop_impl_and_no_default_and_clone() {
        let test_src = r#"
                #[derive(Clone)]
                pub struct TypeUnderTest {
                    pub x: i32,
                    pub y: i32,
                }

                impl Drop for TypeUnderTest {
                    fn drop(&mut self) {}
                }

                impl TypeUnderTest {
                    pub fn pass_by_value() -> Self { unimplemented!() }
                }
            "#;
        test_format_item_struct_with_custom_drop_and_no_default_and_clone(test_src);
    }

    #[test]
    fn test_format_item_struct_with_custom_drop_glue_and_no_default_and_clone() {
        let test_src = r#"
                #![allow(dead_code)]

                // `i32` is present to avoid hitting the ZST checks related to (b/258259459)
                #[derive(Clone)]
                struct StructWithCustomDropImpl(i32);

                impl Drop for StructWithCustomDropImpl {
                    fn drop(&mut self) {
                        println!("dropping!");
                    }
                }

                #[derive(Clone)]
                pub struct TypeUnderTest {
                    field: StructWithCustomDropImpl,
                }

                impl TypeUnderTest {
                    pub fn pass_by_value() -> Self { unimplemented!() }
                }
            "#;
        test_format_item_struct_with_custom_drop_and_no_default_and_clone(test_src);
    }

    #[test]
    fn test_format_item_unsupported_struct_with_custom_drop_and_default_and_nonunpin() {
        let test_src = r#"
                #![feature(negative_impls)]

                #[derive(Default)]
                pub struct SomeStruct {
                    pub x: i32,
                    pub y: i32,
                }

                impl !Unpin for SomeStruct {}

                impl Drop for SomeStruct {
                    fn drop(&mut self) {}
                }

                impl SomeStruct {
                    pub fn pass_by_value() -> Self { unimplemented!() }
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let move_deleted_msg = "C++ moves are deleted \
                                    because there's no non-destructive implementation available.";
            let pass_by_value_msg = "Error generating bindings for `SomeStruct::pass_by_value` \
                        defined at <crubit_unittests.rs>;l=17: \
                 Can't pass the return type by value without a move constructor";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        public:
                          ...
                          __COMMENT__ "Default::default"
                          SomeStruct();

                          __COMMENT__ "Drop::drop"
                          ~SomeStruct();

                          __COMMENT__ #move_deleted_msg
                          SomeStruct(SomeStruct&&) = delete;
                          SomeStruct& operator=(SomeStruct&&) = delete;
                          ...
                          __COMMENT__ #pass_by_value_msg
                          ...
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    ...
                    namespace __crubit_internal {
                    extern "C" void ...(  // `default` thunk decl
                        ::rust_out::SomeStruct* __ret_ptr);
                    }
                    inline SomeStruct::SomeStruct() {
                      __crubit_internal::...(this);
                    }
                    namespace __crubit_internal {
                    extern "C" void ...(  // `drop` thunk decl
                        ::rust_out::SomeStruct& [[clang::annotate_type("lifetime", "__anon1")]]);
                    }
                    inline SomeStruct::~SomeStruct() {
                      __crubit_internal::...(*this);
                    }
                    ...
                }
            );
            assert_cc_not_matches!(result.cc_details.tokens, quote! { pass_by_value });
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    ...
                    #[no_mangle]
                    extern "C" fn ...(
                        __ret_slot: &mut ::core::mem::MaybeUninit<::rust_out::SomeStruct>
                    ) -> () {
                        __ret_slot.write(
                            <::rust_out::SomeStruct as ::core::default::Default>::default());
                    }
                    #[no_mangle]
                    extern "C" fn ...(
                        __self: &mut ::core::mem::MaybeUninit<::rust_out::SomeStruct>
                    ) {
                        unsafe { __self.assume_init_drop() };
                    }
                    ...
                }
            );
            assert_rs_not_matches!(result.rs_details, quote! { pass_by_value });
        });
    }

    /// This test covers how ZSTs (zero-sized-types) are handled.
    /// https://doc.rust-lang.org/reference/items/structs.html refers to this kind of struct as a
    /// "unit-like struct".
    #[test]
    fn test_format_item_unsupported_struct_zero_sized_type_with_no_fields() {
        let test_src = r#"
                pub struct ZeroSizedType1;
                pub struct ZeroSizedType2();
                pub struct ZeroSizedType3{}
            "#;
        for name in ["ZeroSizedType1", "ZeroSizedType2", "ZeroSizedType3"] {
            test_format_item(test_src, name, |result| {
                let err = result.unwrap_err();
                assert_eq!(err, "Zero-sized types (ZSTs) are not supported (b/258259459)");
            });
        }
    }

    #[test]
    fn test_format_item_unsupported_struct_with_only_zero_sized_type_fields() {
        let test_src = r#"
                pub struct ZeroSizedType;
                pub struct SomeStruct {
                    pub zst1: ZeroSizedType,
                    pub zst2: ZeroSizedType,
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Zero-sized types (ZSTs) are not supported (b/258259459)",);
        });
    }

    #[test]
    fn test_format_item_unsupported_struct_with_some_zero_sized_type_fields() {
        let test_src = r#"
                pub struct ZeroSizedType;
                pub struct SomeStruct {
                    pub zst1: ZeroSizedType,
                    pub successful_field: i32,
                    pub zst2: ZeroSizedType,
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let broken_field_msg_zst1 =
                "Skipped bindings for field `zst1`: ZST fields are not supported (b/258259459)";
            let broken_field_msg_zst2 =
                "Skipped bindings for field `zst2`: ZST fields are not supported (b/258259459)";

            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        public:
                            union { ... std::int32_t successful_field; };
                        __COMMENT__ #broken_field_msg_zst1
                        __COMMENT__ #broken_field_msg_zst2
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                    ...
                }
            );

            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeStruct) == 4, ...);
                    static_assert(alignof(SomeStruct) == 4, ...);
                    static_assert(std::is_trivially_destructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_constructible_v<SomeStruct>);
                    static_assert(std::is_trivially_move_assignable_v<SomeStruct>);
                    inline void SomeStruct::__crubit_field_offset_assertions() {
                    static_assert(0 == offsetof(SomeStruct, successful_field));
                    }
                }
            );

            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeStruct>() == 4);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeStruct>() == 4);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, successful_field) == 0);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, zst1) == 4);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeStruct, zst2) == 4);

                }
            );
        });
    }

    #[test]
    fn test_format_item_struct_with_dynamically_sized_field() {
        let test_src = r#"
                #![allow(dead_code)]
                pub struct DynamicallySizedStruct {
                    /// Having a non-ZST field avoids hitting the following error:
                    /// "Zero-sized types (ZSTs) are not supported (b/258259459)"
                    _non_zst_field: f32,
                    _dynamically_sized_field: [i32],
                }
            "#;
        test_format_item(test_src, "DynamicallySizedStruct", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Bindings for dynamically sized types are not supported.");
        });
    }

    #[test]
    fn test_format_item_struct_fields_with_doc_comments() {
        let test_src = r#"
                pub struct SomeStruct {
                    /// Documentation of `successful_field`.
                    pub successful_field: i32,

                    /// Documentation of `unsupported_field`.
                    pub unsupported_field: Option<[i32; 3]>,
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let comment_for_successful_field = " Documentation of `successful_field`.\n\n\
                  Generated from: <crubit_unittests.rs>;l=4";
            let comment_for_unsupported_field = "Field type has been replaced with a blob of bytes: \
                 Generic types are not supported yet (b/259749095)";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... SomeStruct final {
                        ...
                        private:
                            __COMMENT__ #comment_for_unsupported_field
                            unsigned char unsupported_field[16];
                        public:
                            union {
                                __COMMENT__ #comment_for_successful_field
                                std::int32_t successful_field;
                            };
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                    ...
                }
            );
        });
    }

    /// This is a test for an enum that only has `EnumItemDiscriminant` items
    /// (and doesn't have `EnumItemTuple` or `EnumItemStruct` items).  See
    /// also https://doc.rust-lang.org/reference/items/enumerations.html
    #[test]
    fn test_format_item_enum_with_only_discriminant_items() {
        let test_src = r#"
                pub enum SomeEnum {
                    Red,
                    Green = 123,
                    Blue,
                }

                const _: () = assert!(std::mem::size_of::<SomeEnum>() == 1);
                const _: () = assert!(std::mem::align_of::<SomeEnum>() == 1);
            "#;
        test_format_item(test_src, "SomeEnum", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let no_fields_msg = "Field type has been replaced with a blob of bytes: \
                                 No support for bindings of individual `enum` fields";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(1) [[clang::trivial_abi]] SomeEnum final {
                        public:
                            __COMMENT__ "`SomeEnum` doesn't implement the `Default` trait"
                            SomeEnum() = delete;

                            __COMMENT__ "No custom `Drop` impl and no custom \"drop glue\" required"
                            ~SomeEnum() = default;
                            SomeEnum(SomeEnum&&) = default;
                            SomeEnum& operator=(SomeEnum&&) = default;

                            __COMMENT__ "`SomeEnum` doesn't implement the `Clone` trait"
                            SomeEnum(const SomeEnum&) = delete;
                            SomeEnum& operator=(const SomeEnum&) = delete;
                        private:
                            __COMMENT__ #no_fields_msg
                            unsigned char __opaque_blob_of_bytes[1];
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeEnum) == 1, ...);
                    static_assert(alignof(SomeEnum) == 1, ...);
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeEnum>() == 1);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeEnum>() == 1);
                }
            );
        });
    }

    /// This is a test for an enum that has `EnumItemTuple` and `EnumItemStruct`
    /// items. See also https://doc.rust-lang.org/reference/items/enumerations.html
    #[test]
    fn test_format_item_enum_with_tuple_and_struct_items() {
        let test_src = r#"
                pub enum Point {
                    Cartesian(f32, f32),
                    Polar{ dist: f32, angle: f32 },
                }

                const _: () = assert!(std::mem::size_of::<Point>() == 12);
                const _: () = assert!(std::mem::align_of::<Point>() == 4);
            "#;
        test_format_item(test_src, "Point", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let no_fields_msg = "Field type has been replaced with a blob of bytes: \
                                 No support for bindings of individual `enum` fields";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] Point final {
                        public:
                            __COMMENT__ "`Point` doesn't implement the `Default` trait"
                            Point() = delete;

                            __COMMENT__ "No custom `Drop` impl and no custom \"drop glue\" required"
                            ~Point() = default;
                            Point(Point&&) = default;
                            Point& operator=(Point&&) = default;

                            __COMMENT__ "`Point` doesn't implement the `Clone` trait"
                            Point(const Point&) = delete;
                            Point& operator=(const Point&) = delete;
                        private:
                            __COMMENT__ #no_fields_msg
                            unsigned char __opaque_blob_of_bytes[12];
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(Point) == 12, ...);
                    static_assert(alignof(Point) == 4, ...);
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::Point>() == 12);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::Point>() == 4);
                }
            );
        });
    }

    /// This test covers how zero-variant enums are handled.  See also
    /// https://doc.rust-lang.org/reference/items/enumerations.html#zero-variant-enums
    #[test]
    fn test_format_item_unsupported_enum_zero_variants() {
        let test_src = r#"
                pub enum ZeroVariantEnum {}
            "#;
        test_format_item(test_src, "ZeroVariantEnum", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Zero-sized types (ZSTs) are not supported (b/258259459)");
        });
    }

    /// This is a test for a `union`.  See also
    /// https://doc.rust-lang.org/reference/items/unions.html
    #[test]
    fn test_format_item_union() {
        let test_src = r#"
                pub union SomeUnion {
                    pub i: i32,
                    pub f: f64,
                }

                const _: () = assert!(std::mem::size_of::<SomeUnion>() == 8);
                const _: () = assert!(std::mem::align_of::<SomeUnion>() == 8);
            "#;
        test_format_item(test_src, "SomeUnion", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    union CRUBIT_INTERNAL_RUST_TYPE(...) alignas(8) [[clang::trivial_abi]] SomeUnion final {
                        public:
                            __COMMENT__ "`SomeUnion` doesn't implement the `Default` trait"
                            SomeUnion() = delete;

                            __COMMENT__ "No custom `Drop` impl and no custom \"drop glue\" required"
                            ~SomeUnion() = default;
                            SomeUnion(SomeUnion&&) = default;
                            SomeUnion& operator=(SomeUnion&&) = default;

                            __COMMENT__ "`SomeUnion` doesn't implement the `Clone` trait"
                            SomeUnion(const SomeUnion&) = delete;
                            SomeUnion& operator=(const SomeUnion&) = delete;
                        ...
                        struct {
                            ...
                            std::int32_t value;
                        } i;
                        ...
                        struct {
                            ...
                            double value;
                        } f;
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeUnion) == 8, ...);
                    static_assert(alignof(SomeUnion) == 8, ...);
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeUnion>() == 8);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeUnion>() == 8);
                }
            );
        });
    }

    #[test]
    fn test_format_item_doc_comments_union() {
        let test_src = r#"
            /// Doc for some union.
            pub union SomeUnionWithDocs {
                /// Doc for a field in a union.
                pub i: i32,
                pub f: f64
            }
        "#;
        test_format_item(test_src, "SomeUnionWithDocs", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let comment = " Doc for some union.\n\n\
                           Generated from: <crubit_unittests.rs>;l=3";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    __COMMENT__ #comment
                    union ... SomeUnionWithDocs final {
                        ...
                    }
                    ...
                }
            );
        });
    }

    #[test]
    fn test_format_item_doc_comments_enum() {
        let test_src = r#"
            /** Doc for some enum. */
            pub enum SomeEnumWithDocs {
                Kind1(i32),
            }
        "#;
        test_format_item(test_src, "SomeEnumWithDocs", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let comment = " Doc for some enum. \n\n\
                            Generated from: <crubit_unittests.rs>;l=3";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    __COMMENT__ #comment
                    struct ... SomeEnumWithDocs final {
                        ...
                    }
                    ...
                }
            );
        });
    }

    #[test]
    fn test_format_item_doc_comments_struct() {
        let test_src = r#"
            #![allow(dead_code)]
            #[doc = "Doc for some struct."]
            pub struct SomeStructWithDocs {
                #[doc = "Doc for first field."]
                some_field : i32,
            }
        "#;
        test_format_item(test_src, "SomeStructWithDocs", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let comment = "Doc for some struct.\n\n\
                           Generated from: <crubit_unittests.rs>;l=4";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    __COMMENT__ #comment
                    struct ... SomeStructWithDocs final {
                        ...
                    }
                    ...
                }
            );
        });
    }

    #[test]
    fn test_format_item_doc_comments_tuple_struct() {
        let test_src = r#"
            #![allow(dead_code)]

            /// Doc for some tuple struct.
            pub struct SomeTupleStructWithDocs(i32);
        "#;
        test_format_item(test_src, "SomeTupleStructWithDocs", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let comment = " Doc for some tuple struct.\n\n\
                           Generated from: <crubit_unittests.rs>;l=5";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    __COMMENT__ #comment
                    struct ... SomeTupleStructWithDocs final {
                        ...
                    }
                    ...
                },
            );
        });
    }

    #[test]
    fn test_format_item_source_loc_macro_rules() {
        let test_src = r#"
            #![allow(dead_code)]

            macro_rules! some_tuple_struct_macro_for_testing_source_loc {
                () => {
                    /// Some doc on SomeTupleStructMacroForTesingSourceLoc.
                    pub struct SomeTupleStructMacroForTesingSourceLoc(i32);
                };
            }

            some_tuple_struct_macro_for_testing_source_loc!();
        "#;
        test_format_item(test_src, "SomeTupleStructMacroForTesingSourceLoc", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let source_loc_comment = " Some doc on SomeTupleStructMacroForTesingSourceLoc.\n\n\
                                      Generated from: <crubit_unittests.rs>;l=7";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    __COMMENT__ #source_loc_comment
                    struct ... SomeTupleStructMacroForTesingSourceLoc final {
                        ...
                    }
                    ...
                },
            );
        });
    }

    #[test]
    fn test_format_item_source_loc_with_no_doc_comment() {
        let test_src = r#"
            #![allow(dead_code)]

            pub struct SomeTupleStructWithNoDocComment(i32);
        "#;
        test_format_item(test_src, "SomeTupleStructWithNoDocComment", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            let comment = "Generated from: <crubit_unittests.rs>;l=4";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    __COMMENT__ #comment
                    struct ... SomeTupleStructWithNoDocComment final {
                        ...
                    }
                    ...
                },
            );
        });
    }

    #[test]
    fn test_format_item_unsupported_static_value() {
        let test_src = r#"
                #[no_mangle]
                pub static STATIC_VALUE: i32 = 42;
            "#;
        test_format_item(test_src, "STATIC_VALUE", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Unsupported rustc_hir::hir::ItemKind: static item");
        });
    }

    #[test]
    fn test_format_item_unsupported_const_value() {
        let test_src = r#"
                pub const CONST_VALUE: i32 = 42;
            "#;
        test_format_item(test_src, "CONST_VALUE", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "Unsupported rustc_hir::hir::ItemKind: constant item");
        });
    }

    #[test]
    fn test_format_item_use_normal_type() {
        let test_src = r#"
            pub mod test_mod {
                pub struct S{
                    pub field: i32
                }
            }

            pub use test_mod::S as G;
            "#;
        test_format_item(test_src, "G", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    using G = ::rust_out::test_mod::S;
                }
            );
        });
    }

    #[test]
    fn test_format_item_type_alias() {
        let test_src = r#"
                pub type TypeAlias = i32;
            "#;
        test_format_item(test_src, "TypeAlias", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    using TypeAlias = std::int32_t;
                }
            );
        });
    }

    #[test]
    fn test_format_item_type_alias_should_give_underlying_type() {
        let test_src = r#"
                pub type TypeAlias1 = i32;
                pub type TypeAlias2 = TypeAlias1;
            "#;
        test_format_item(test_src, "TypeAlias2", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    using TypeAlias2 = std::int32_t;
                }
            );
        });
    }

    #[test]
    fn test_format_item_private_type_alias_wont_generate_bindings() {
        let test_src = r#"
            #[allow(dead_code)]
            type TypeAlias = i32;
            "#;
        test_format_item(test_src, "TypeAlias", |result| {
            let result = result.unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    fn test_format_item_pub_type_alias_on_private_type_wont_generate_bindings() {
        let test_src = r#"
            #![allow(private_interfaces)]
            struct SomeStruct;
            pub type TypeAlias = SomeStruct;
            "#;
        test_format_item(test_src, "TypeAlias", |result| {
            let err = result.unwrap_err();
            assert_eq!(
                err,
                "Not directly public type (re-exports are not supported yet - b/262052635)"
            );
        });
    }

    #[test]
    fn test_format_item_unsupported_generic_type_alias() {
        let test_src = r#"
            pub type TypeAlias<T> = T;
            "#;
        test_format_item(test_src, "TypeAlias", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "The following Rust type is not supported yet: T");
        });
    }

    #[test]
    fn test_format_item_unsupported_type_without_direct_existence() {
        let test_src = r#"
            pub trait Evil {
                type Type;
            }

            const _ : () = {
                pub struct NamelessType;
                impl Evil for i64 {
                    type Type = NamelessType;
                }
            };
            pub type EvilAlias = <i64 as Evil>::Type;
            "#;
        test_format_item(test_src, "EvilAlias", |result| {
            let err = result.unwrap_err();
            assert_eq!(err, "The following Rust type is not supported yet: <i64 as Evil>::Type");
        });
    }

    #[test]
    fn test_format_item_unsupported_impl_item_const_value() {
        let test_src = r#"
                #![allow(dead_code)]

                pub struct SomeStruct(i32);

                impl SomeStruct {
                    pub const CONST_VALUE: i32 = 42;
                }
            "#;
        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            let unsupported_msg = "Error generating bindings for `SomeStruct::CONST_VALUE` \
                                   defined at <crubit_unittests.rs>;l=7: \
                                   Unsupported `impl` item kind: Const";
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] SomeStruct final {
                        ...
                        __COMMENT__ #unsupported_msg
                        ...
                    };
                    ...
                }
            );
        });
    }

    #[test]
    fn test_format_item_generate_bindings_for_top_level_type_alias() {
        let test_src = r#"
            #![feature(inherent_associated_types)]
            #![allow(incomplete_features)]
            #![allow(dead_code)]
            pub struct Evil {
                dumb: i32,
            }

            impl Evil {
                pub type Type = i64;
            }
            pub type EvilAlias = Evil::Type;
        "#;
        test_format_item(test_src, "Evil", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_not_matches!(
                main_api.tokens,
                quote! {
                    std::int64_t
                }
            );
        });
    }

    /// `test_format_ret_ty_for_cc_successes` provides test coverage for cases
    /// where `format_ty_for_cc` takes `TypeLocation::FnReturn` and returns
    /// an `Ok(...)`.  Additional testcases are covered by
    /// `test_format_ty_for_cc_successes`.
    #[test]
    fn test_format_ret_ty_for_cc_successes() {
        let testcases = [
            // ( <Rust type>, <expected C++ type> )
            ("bool", "bool"), // TyKind::Bool
            ("()", "void"),
            // TODO(b/254507801): Expect `crubit::Never` instead (see the bug for more
            // details).
            ("!", "void"),
            (
                "extern \"C\" fn (f32, f32) -> f32",
                "crubit :: type_identity_t < float (float , float) > &",
            ),
        ];
        test_ty(TypeLocation::FnReturn, &testcases, quote! {}, |desc, tcx, ty, expected| {
            let actual = {
                let db = bindings_db_for_tests(tcx);
                let cc_snippet = format_ty_for_cc(&db, ty, TypeLocation::FnReturn).unwrap();
                cc_snippet.tokens.to_string()
            };
            let expected = expected.parse::<TokenStream>().unwrap().to_string();
            assert_eq!(actual, expected, "{desc}");
        });
    }

    /// `test_format_ty_for_cc_successes` provides test coverage for cases where
    /// `format_ty_for_cc` returns an `Ok(...)`.
    ///
    /// Note that using `std::int8_t` (instead of `::std::int8_t`) has been an
    /// explicit decision. The "Google C++ Style Guide" suggests to "avoid
    /// nested namespaces that match well-known top-level namespaces" and "in
    /// particular, [...] not create any nested std namespaces.".  It
    /// seems desirable if the generated bindings conform to this aspect of the
    /// style guide, because it makes things easier for *users* of these
    /// bindings.
    #[test]
    fn test_format_ty_for_cc_successes() {
        let testcases = [
            // ( <Rust type>, (<expected C++ type>,
            //                 <expected #include>,
            //                 <expected prereq def>,
            //                 <expected prereq fwd decl>) )
            ("bool", ("bool", "", "", "")),
            ("f32", ("float", "", "", "")),
            ("f64", ("double", "", "", "")),
            ("i8", ("std::int8_t", "<cstdint>", "", "")),
            ("i16", ("std::int16_t", "<cstdint>", "", "")),
            ("i32", ("std::int32_t", "<cstdint>", "", "")),
            ("i64", ("std::int64_t", "<cstdint>", "", "")),
            ("isize", ("std::intptr_t", "<cstdint>", "", "")),
            ("u8", ("std::uint8_t", "<cstdint>", "", "")),
            ("u16", ("std::uint16_t", "<cstdint>", "", "")),
            ("u32", ("std::uint32_t", "<cstdint>", "", "")),
            ("u64", ("std::uint64_t", "<cstdint>", "", "")),
            ("usize", ("std::uintptr_t", "<cstdint>", "", "")),
            ("char", ("rs_std::rs_char", "<crubit/support/for/tests/rs_std/rs_char.h>", "", "")),
            ("SomeStruct", ("::rust_out::SomeStruct", "", "SomeStruct", "")),
            ("SomeEnum", ("::rust_out::SomeEnum", "", "SomeEnum", "")),
            ("SomeUnion", ("::rust_out::SomeUnion", "", "SomeUnion", "")),
            ("OriginallyCcStruct", ("cc_namespace :: CcStruct", "", "OriginallyCcStruct", "")),
            ("*const i32", ("std :: int32_t const *", "<cstdint>", "", "")),
            ("*mut i32", ("std::int32_t*", "<cstdint>", "", "")),
            (
                "&'static i32",
                (
                    "std :: int32_t const & [[clang :: annotate_type (\"lifetime\" , \"static\")]]",
                    "<cstdint>",
                    "",
                    "",
                ),
            ),
            (
                "&'static mut i32",
                (
                    "std :: int32_t & [[clang :: annotate_type (\"lifetime\" , \"static\")]]",
                    "<cstdint>",
                    "",
                    "",
                ),
            ),
            // `SomeStruct` is a `fwd_decls` prerequisite (not `defs` prerequisite):
            ("*mut SomeStruct", ("::rust_out::SomeStruct*", "", "", "SomeStruct")),
            // Testing propagation of deeper/nested `fwd_decls`:
            ("*mut *mut SomeStruct", (":: rust_out :: SomeStruct * *", "", "", "SomeStruct")),
            // Testing propagation of `const` / `mut` qualifiers:
            ("*mut *const f32", ("float const * *", "", "", "")),
            ("*const *mut f32", ("float * const *", "", "", "")),
            (
                // Rust function pointers are non-nullable, so when function pointers are used as a
                // parameter type (i.e. in `TypeLocation::FnParam`) then we can translate to
                // generate a C++ function *reference*, rather than a C++ function *pointer*.
                "extern \"C\" fn (f32, f32) -> f32",
                (
                    "crubit :: type_identity_t < float (float , float) > &",
                    "<crubit/support/for/tests/internal/cxx20_backports.h>",
                    "",
                    "",
                ),
            ),
            // Unsafe extern "C" function pointers are, to C++, just function pointers.
            (
                "unsafe extern \"C\" fn(f32, f32) -> f32",
                (
                    "crubit :: type_identity_t < float (float , float) > &",
                    "<crubit/support/for/tests/internal/cxx20_backports.h>",
                    "",
                    "",
                ),
            ),
            (
                // Nested function pointer (i.e. `TypeLocation::Other`) means that
                // we need to generate a C++ function *pointer*, rather than a C++
                // function *reference*.
                "*const extern \"C\" fn (f32, f32) -> f32",
                (
                    "crubit :: type_identity_t < float (float , float) > * const *",
                    "<crubit/support/for/tests/internal/cxx20_backports.h>",
                    "",
                    "",
                ),
            ),
            // Extra parens/sugar are expected to be ignored:
            ("(bool)", ("bool", "", "", "")),
        ];
        let preamble = quote! {
            #![allow(unused_parens)]
            #![feature(register_tool)]
            #![register_tool(__crubit)]

            pub struct SomeStruct {
                pub x: i32,
                pub y: i32,
            }
            pub enum SomeEnum {
                Cartesian{x: f64, y: f64},
                Polar{angle: f64, dist: f64},
            }
            pub union SomeUnion {
                pub x: i32,
                pub y: i32,
            }

            #[__crubit::annotate(cc_type = "cc_namespace::CcStruct")]
            pub struct OriginallyCcStruct {
                pub x: i32
            }
        };
        test_ty(
            TypeLocation::FnParam,
            &testcases,
            preamble,
            |desc, tcx, ty,
             (expected_tokens, expected_include, expected_prereq_def, expected_prereq_fwd_decl)| {
                let (actual_tokens, actual_prereqs) = {
                    let db = bindings_db_for_tests(tcx);
                    let s = format_ty_for_cc(&db, ty, TypeLocation::FnParam).unwrap();
                    (s.tokens.to_string(), s.prereqs)
                };
                let (actual_includes, actual_prereq_defs, actual_prereq_fwd_decls) =
                    (actual_prereqs.includes, actual_prereqs.defs, actual_prereqs.fwd_decls);

                let expected_tokens = expected_tokens.parse::<TokenStream>().unwrap().to_string();
                assert_eq!(actual_tokens, expected_tokens, "{desc}");

                if expected_include.is_empty() {
                    assert!(
                        actual_includes.is_empty(),
                        "{desc}: `actual_includes` is unexpectedly non-empty: {actual_includes:?}",
                    );
                } else {
                    let expected_include: TokenStream = expected_include.parse().unwrap();
                    assert_cc_matches!(
                        format_cc_includes(&actual_includes),
                        quote! { __HASH_TOKEN__ include #expected_include }
                    );
                }

                if expected_prereq_def.is_empty() {
                    assert!(
                        actual_prereq_defs.is_empty(),
                        "{desc}: `actual_prereq_defs` is unexpectedly non-empty",
                    );
                } else {
                    let expected_def_id = find_def_id_by_name(tcx, expected_prereq_def);
                    assert_eq!(1, actual_prereq_defs.len());
                    assert_eq!(expected_def_id, actual_prereq_defs.into_iter().next().unwrap());
                }

                if expected_prereq_fwd_decl.is_empty() {
                    assert!(
                        actual_prereq_fwd_decls.is_empty(),
                        "{desc}: `actual_prereq_fwd_decls` is unexpectedly non-empty",
                    );
                } else {
                    let expected_def_id = find_def_id_by_name(tcx, expected_prereq_fwd_decl);
                    assert_eq!(1, actual_prereq_fwd_decls.len());
                    assert_eq!(expected_def_id,
                               actual_prereq_fwd_decls.into_iter().next().unwrap());
                }
            },
        );
    }

    /// `test_format_ty_for_cc_failures` provides test coverage for cases where
    /// `format_ty_for_cc` returns an `Err(...)`.
    ///
    /// It seems okay to have no test coverage for now for the following types
    /// (which should never be encountered when generating bindings and where
    /// `format_ty_for_cc` should panic):
    /// - TyKind::Closure
    /// - TyKind::Error
    /// - TyKind::FnDef
    /// - TyKind::Infer
    ///
    /// TODO(lukasza): Add test coverage (here and in the "for_rs" flavours)
    /// for:
    /// - TyKind::Bound
    /// - TyKind::Dynamic (`dyn Eq`)
    /// - TyKind::Foreign (`extern type T`)
    /// - https://doc.rust-lang.org/beta/unstable-book/language-features/generators.html:
    ///   TyKind::Generator, TyKind::GeneratorWitness
    /// - TyKind::Param
    /// - TyKind::Placeholder
    #[test]
    fn test_format_ty_for_cc_failures() {
        let testcases = [
            // ( <Rust type>, <expected error message> )
            (
                "()", // Empty TyKind::Tuple
                "`()` / `void` is only supported as a return type (b/254507801)",
            ),
            (
                // TODO(b/254507801): Expect `crubit::Never` instead (see the bug for more
                // details).
                "!", // TyKind::Never
                "The never type `!` is only supported as a return type (b/254507801)",
            ),
            (
                "(i32, i32)", // Non-empty TyKind::Tuple
                "Tuples are not supported yet: (i32, i32) (b/254099023)",
            ),
            (
                "&'static &'static i32", // TyKind::Ref (nested reference - referent of reference)
                "Failed to format the referent of the reference type `&'static &'static i32`: \
                 Can't format `&'static i32`, because references are only supported \
                 in function parameter types and return types (b/286256327)",
            ),
            (
                "extern \"C\" fn (&i32)", // TyKind::Ref (nested reference - underneath fn ptr)
                "Generic functions are not supported yet (b/259749023)",
            ),
            (
                "[i32; 42]", // TyKind::Array
                "The following Rust type is not supported yet: [i32; 42]",
            ),
            (
                "&'static [i32]", // TyKind::Slice (nested underneath TyKind::Ref)
                "Failed to format the referent of the reference type `&'static [i32]`: \
                 The following Rust type is not supported yet: [i32]",
            ),
            (
                "&'static str", // TyKind::Str (nested underneath TyKind::Ref)
                "Failed to format the referent of the reference type `&'static str`: \
                 The following Rust type is not supported yet: str",
            ),
            (
                "impl Eq", // TyKind::Alias
                "The following Rust type is not supported yet: impl Eq",
            ),
            (
                "fn(i32) -> i32", // TyKind::FnPtr (default ABI = "Rust")
                "Function pointers can't have a thunk: \
                 Calling convention other than `extern \"C\"` requires a thunk",
            ),
            (
                "extern \"C\" fn (SomeStruct, f32) -> f32",
                "Function pointers can't have a thunk: Type of parameter #0 requires a thunk",
            ),
            (
                "extern \"C\" fn (f32, f32) -> SomeStruct",
                "Function pointers can't have a thunk: Return type requires a thunk",
            ),
            // TODO(b/254094650): Consider mapping this to Clang's (and GCC's) `__int128`
            // or to `absl::in128`.
            ("i128", "C++ doesn't have a standard equivalent of `i128` (b/254094650)"),
            ("u128", "C++ doesn't have a standard equivalent of `u128` (b/254094650)"),
            ("ConstGenericStruct<42>", "Generic types are not supported yet (b/259749095)"),
            ("TypeGenericStruct<u8>", "Generic types are not supported yet (b/259749095)"),
            (
                // This double-checks that TyKind::Adt(..., substs) are present
                // even if the type parameter argument is not explicitly specified
                // (here it comes from the default: `...Struct<T = u8>`).
                "TypeGenericStruct",
                "Generic types are not supported yet (b/259749095)",
            ),
            ("LifetimeGenericStruct<'static>", "Generic types are not supported yet (b/259749095)"),
            (
                "std::cmp::Ordering",
                "Type `std::cmp::Ordering` comes from the `core` crate, \
                 but no `--bindings-from-dependency` was specified for this crate",
            ),
            ("Option<i8>", "Generic types are not supported yet (b/259749095)"),
            (
                "PublicReexportOfStruct",
                "Not directly public type (re-exports are not supported yet - b/262052635)",
            ),
            (
                // This testcase is like `PublicReexportOfStruct`, but the private type and the
                // re-export are in another crate.  When authoring this test
                // `core::alloc::LayoutError` was a public re-export of
                // `core::alloc::layout::LayoutError`:
                // `https://play.rust-lang.org/?version=stable&mode=debug&edition=2021&gist=d2b5528af9b33b25abe44cc4646d65e3`
                // TODO(b/258261328): Once cross-crate bindings are supported we should try
                // to test them via a test crate that we control (rather than testing via
                // implementation details of the std crate).
                "core::alloc::LayoutError",
                "Not directly public type (re-exports are not supported yet - b/262052635)",
            ),
            (
                "*const Option<i8>",
                "Failed to format the pointee \
                 of the pointer type `*const std::option::Option<i8>`: \
                 Generic types are not supported yet (b/259749095)",
            ),
        ];
        let preamble = quote! {
            #![feature(never_type)]

            #[repr(C)]
            pub struct SomeStruct {
                pub x: i32,
                pub y: i32,
            }

            pub struct ConstGenericStruct<const N: usize> {
                pub arr: [u8; N],
            }

            pub struct TypeGenericStruct<T = u8> {
                pub t: T,
            }

            pub struct LifetimeGenericStruct<'a> {
                pub reference: &'a u8,
            }

            mod private_submodule {
                pub struct PublicStructInPrivateModule;
            }
            pub use private_submodule::PublicStructInPrivateModule
                as PublicReexportOfStruct;
        };
        test_ty(TypeLocation::FnParam, &testcases, preamble, |desc, tcx, ty, expected_msg| {
            let db = bindings_db_for_tests(tcx);
            let anyhow_err = format_ty_for_cc(&db, ty, TypeLocation::FnParam)
                .expect_err(&format!("Expecting error for: {desc}"));
            let actual_msg = format!("{anyhow_err:#}");
            assert_eq!(&actual_msg, *expected_msg, "{desc}");
        });
    }

    #[test]
    fn test_format_ty_for_rs_successes() {
        // Test coverage for cases where `format_ty_for_rs` returns an `Ok(...)`.
        let testcases = [
            // ( <Rust type>, <expected Rust spelling for ..._cc_api_impl.rs> )
            ("bool", "bool"),
            ("f32", "f32"),
            ("f64", "f64"),
            ("i8", "i8"),
            ("i16", "i16"),
            ("i32", "i32"),
            ("i64", "i64"),
            ("i128", "i128"),
            ("isize", "isize"),
            ("u8", "u8"),
            ("u16", "u16"),
            ("u32", "u32"),
            ("u64", "u64"),
            ("u128", "u128"),
            ("usize", "usize"),
            ("char", "char"),
            ("!", "!"),
            ("()", "()"),
            // ADTs:
            ("SomeStruct", "::rust_out::SomeStruct"),
            ("SomeEnum", "::rust_out::SomeEnum"),
            ("SomeUnion", "::rust_out::SomeUnion"),
            // Type from another crate:
            ("std::cmp::Ordering", "::core::cmp::Ordering"),
            // `const` and `mut` pointers:
            ("*const i32", "*const i32"),
            ("*mut i32", "*mut i32"),
            // References:
            ("&i32", "& '__anon1 i32"),
            ("&mut i32", "& '__anon1 mut i32"),
            ("&'_ i32", "& '__anon1 i32"),
            ("&'static i32", "& 'static i32"),
            // Pointer to an ADT:
            ("*mut SomeStruct", "* mut :: rust_out :: SomeStruct"),
            ("extern \"C\" fn(i32) -> i32", "extern \"C\" fn(i32) -> i32"),
        ];
        let preamble = quote! {
            #![feature(never_type)]

            pub struct SomeStruct {
                pub x: i32,
                pub y: i32,
            }
            pub enum SomeEnum {
                Cartesian{x: f64, y: f64},
                Polar{angle: f64, dist: f64},
            }
            pub union SomeUnion {
                pub x: i32,
                pub y: i32,
            }
        };
        test_ty(TypeLocation::FnParam, &testcases, preamble, |desc, tcx, ty, expected_tokens| {
            let actual_tokens = format_ty_for_rs(tcx, ty).unwrap().to_string();
            let expected_tokens = expected_tokens.parse::<TokenStream>().unwrap().to_string();
            assert_eq!(actual_tokens, expected_tokens, "{desc}");
        });
    }

    #[test]
    fn test_format_ty_for_rs_failures() {
        // This test provides coverage for cases where `format_ty_for_rs` returns an
        // `Err(...)`.
        let testcases = [
            // ( <Rust type>, <expected error message> )
            (
                "(i32, i32)", // Non-empty TyKind::Tuple
                "Tuples are not supported yet: (i32, i32) (b/254099023)",
            ),
            (
                "[i32; 42]", // TyKind::Array
                "The following Rust type is not supported yet: [i32; 42]",
            ),
            (
                "&'static [i32]", // TyKind::Slice (nested underneath TyKind::Ref)
                "Failed to format the referent of the reference type `&'static [i32]`: \
                 The following Rust type is not supported yet: [i32]",
            ),
            (
                "&'static str", // TyKind::Str (nested underneath TyKind::Ref)
                "Failed to format the referent of the reference type `&'static str`: \
                 The following Rust type is not supported yet: str",
            ),
            (
                "impl Eq", // TyKind::Alias
                "The following Rust type is not supported yet: impl Eq",
            ),
            (
                "Option<i8>", // TyKind::Adt - generic + different crate
                "Generic types are not supported yet (b/259749095)",
            ),
        ];
        let preamble = quote! {};
        test_ty(TypeLocation::FnParam, &testcases, preamble, |desc, tcx, ty, expected_err| {
            let anyhow_err =
                format_ty_for_rs(tcx, ty).expect_err(&format!("Expecting error for: {desc}"));
            let actual_err = format!("{anyhow_err:#}");
            assert_eq!(&actual_err, *expected_err, "{desc}");
        });
    }

    #[test]
    fn test_format_namespace_bound_cc_tokens() {
        run_compiler_for_testing("", |tcx| {
            let top_level = NamespaceQualifier::new::<&str>([]);
            let m1 = NamespaceQualifier::new(["m1"]);
            let m2 = NamespaceQualifier::new(["m2"]);
            let input = [
                (None, top_level.clone(), quote! { void f0a(); }),
                (None, m1.clone(), quote! { void f1a(); }),
                (None, m1.clone(), quote! { void f1b(); }),
                (None, top_level.clone(), quote! { void f0b(); }),
                (None, top_level.clone(), quote! { void f0c(); }),
                (None, m2.clone(), quote! { void f2a(); }),
                (None, m1.clone(), quote! { void f1c(); }),
                (None, m1.clone(), quote! { void f1d(); }),
            ];
            assert_cc_matches!(
                format_namespace_bound_cc_tokens(input, tcx),
                quote! {
                    void f0a();

                    namespace m1 {
                    void f1a();
                    void f1b();
                    }  // namespace m1

                    void f0b();
                    void f0c();

                    namespace m2 {
                    void f2a();
                    }

                    namespace m1 {
                    void f1c();
                    void f1d();
                    }  // namespace m1
                },
            );
        });
    }

    #[test]
    fn test_format_namespace_bound_cc_tokens_with_reserved_cpp_keywords() {
        run_compiler_for_testing("", |tcx| {
            let working_module = NamespaceQualifier::new(["foo", "working_module", "bar"]);
            let broken_module = NamespaceQualifier::new(["foo", "reinterpret_cast", "bar"]);
            let input = vec![
                (None, broken_module.clone(), quote! { void broken_module_f1(); }),
                (None, broken_module.clone(), quote! { void broken_module_f2(); }),
                (None, working_module.clone(), quote! { void working_module_f3(); }),
                (None, working_module.clone(), quote! { void working_module_f4(); }),
                (None, broken_module.clone(), quote! { void broken_module_f5(); }),
                (None, broken_module.clone(), quote! { void broken_module_f6(); }),
                (None, working_module.clone(), quote! { void working_module_f7(); }),
                (None, working_module.clone(), quote! { void working_module_f8(); }),
            ];
            let broken_module_msg = "Failed to format namespace name `foo::reinterpret_cast::bar`: \
                                    `reinterpret_cast` is a C++ reserved keyword \
                                    and can't be used as a C++ identifier";
            assert_cc_matches!(
                format_namespace_bound_cc_tokens(input, tcx),
                quote! {
                    __COMMENT__ #broken_module_msg

                    namespace foo::working_module::bar {
                    void working_module_f3();
                    void working_module_f4();
                    }  // namespace foo::working_module::bar

                    // TODO(lukasza): Repeating the error message below seems somewhat undesirable.
                    // OTOH fixing this seems low priority, given that errors when formatting
                    // namespace names should be fairly rare.  And fixing this requires extra work
                    // and effort, especially if we want to:
                    // 1) coalesce the 2 chunks of the `working_module`
                    // 2) avoid reordering where the `broken_module` error comment appears.
                    __COMMENT__ #broken_module_msg

                    namespace foo::working_module::bar {
                    void working_module_f7();
                    void working_module_f8();
                    }  // namespace foo::working_module::bar
                },
            );
        });
    }

    #[test]
    fn test_must_use_attr_for_fn_no_msg() {
        let test_src = r#"
        #[must_use]
        pub fn add(x: i32, y: i32) -> i32 {
            x + y
        }"#;

        test_format_item(test_src, "add", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    [[nodiscard]] std::int32_t add(std::int32_t x, std::int32_t y);
                }
            )
        })
    }

    #[test]
    fn test_must_use_attr_for_fn_msg() {
        let test_src = r#"
        #[must_use = "hello!"]
        pub fn add(x: i32, y: i32) -> i32 {
            x + y
        }"#;

        test_format_item(test_src, "add", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    [[nodiscard("hello!")]] std::int32_t add(std::int32_t x, std::int32_t y);
                }
            )
        })
    }

    #[test]
    fn test_must_use_attr_for_struct_no_msg() {
        let test_src = r#"
        #[must_use]
        pub struct SomeStruct {
            pub x: u32,
            pub y: u32,
        }"#;

        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... [[nodiscard]] ... SomeStruct final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_must_use_attr_for_struct_msg() {
        let test_src = r#"
        #[must_use = "foo"]
        pub struct SomeStruct {
            pub x: u32,
            pub y: u32,
        }"#;

        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... [[nodiscard("foo")]] ... SomeStruct final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_must_use_attr_for_enum_no_msg() {
        let test_src = r#"
        #[must_use]
        pub enum SomeEnum {
            A(i32),
            B(u32),
        }"#;

        test_format_item(test_src, "SomeEnum", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... [[nodiscard]] ... SomeEnum final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_must_use_attr_for_enum_msg() {
        let test_src = r#"
        #[must_use = "foo"]
        pub enum SomeEnum {
            A(i32),
            B(u32),
        }"#;

        test_format_item(test_src, "SomeEnum", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    struct ... [[nodiscard("foo")]] ... SomeEnum final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_must_use_attr_for_union_no_msg() {
        let test_src = r#"
        #[must_use]
        pub union SomeUnion {
            pub x: u32,
            pub y: u32,
        }"#;

        test_format_item(test_src, "SomeUnion", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    union ... [[nodiscard]] ... SomeUnion final {
                        ...
                    };
                }
            )
        })
    }
    #[test]
    fn test_must_use_attr_for_union_msg() {
        let test_src = r#"
        #[must_use = "foo"]
        pub union SomeUnion {
            pub x: u32,
            pub y: u32,
        }"#;

        test_format_item(test_src, "SomeUnion", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    union ... [[nodiscard("foo")]] ... SomeUnion final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_fn_no_args() {
        let test_src = r#"
        #[deprecated]
        pub fn add(x: i32, y: i32) -> i32 {
            x + y
        }"#;

        test_format_item(test_src, "add", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    [[deprecated]] std::int32_t add(std::int32_t x, std::int32_t y);
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_fn_with_message() {
        let test_src = r#"
        #[deprecated = "Use add_i32 instead"]
        pub fn add(x: i32, y: i32) -> i32 {
            x + y
        }"#;

        test_format_item(test_src, "add", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    [[deprecated("Use add_i32 instead")]] std::int32_t add(std::int32_t x, std::int32_t y);
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_fn_with_named_args() {
        let test_src = r#"
        #[deprecated(since = "3.14", note = "Use add_i32 instead")]
        pub fn add(x: i32, y: i32) -> i32 {
            x + y
        }"#;

        test_format_item(test_src, "add", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    [[deprecated("Use add_i32 instead")]] std::int32_t add(std::int32_t x, std::int32_t y);
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_struct_no_args() {
        let test_src = r#"
        #[deprecated]
        pub struct SomeStruct {
            pub x: u32,
            pub y: u32,
        }"#;

        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    struct ... [[deprecated]] ... SomeStruct final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_struct_with_message() {
        let test_src = r#"
        #[deprecated = "Use AnotherStruct instead"]
        pub struct SomeStruct {
            pub x: u32,
            pub y: u32,
        }"#;

        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    struct ... [[deprecated("Use AnotherStruct instead")]] ... SomeStruct final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_struct_with_named_args() {
        let test_src = r#"
        #[deprecated(since = "3.14", note = "Use AnotherStruct instead")]
        pub struct SomeStruct {
            pub x: u32,
            pub y: u32,
        }"#;

        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    struct ... [[deprecated("Use AnotherStruct instead")]] ... SomeStruct final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_union_with_named_args() {
        let test_src = r#"
        #[deprecated(since = "3.14", note = "Use AnotherUnion instead")]
        pub struct SomeUnion {
            pub x: u32,
            pub y: u32,
        }"#;

        test_format_item(test_src, "SomeUnion", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    struct ... [[deprecated("Use AnotherUnion instead")]] ... SomeUnion final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_enum_with_named_args() {
        let test_src = r#"
        #[deprecated(since = "3.14", note = "Use AnotherEnum instead")]
        pub enum SomeEnum {
            Integer(i32),
            FloatingPoint(f64),
        }"#;

        test_format_item(test_src, "SomeEnum", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    struct ... [[deprecated("Use AnotherEnum instead")]] ... SomeEnum final {
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_struct_fields() {
        let test_src = r#"
        pub struct SomeStruct {
            #[deprecated = "Use `y` instead"]
            pub x: u32,

            pub y: u32,
        }"#;

        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    struct ... SomeStruct final {
                        ...
                        union {
                            ...
                            [[deprecated("Use `y` instead")]] std::uint32_t x;
                        }
                        ...
                        union {
                            ...
                            std::uint32_t y;
                        }
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_deprecated_attr_for_impl_block() {
        let test_src = r#"
        pub struct SomeStruct {
            pub x: u32,
            pub y: u32,
        }

        #[deprecated = "Use AnotherStruct instead"]
        impl SomeStruct {
            pub fn sum(&self) -> u32 {
                self.x + self.y
            }

            pub fn product(&self) -> u32 {
                self.x * self.y
            }
        }"#;

        test_format_item(test_src, "SomeStruct", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    struct ... SomeStruct final {
                        ...
                        ... [[deprecated("Use AnotherStruct instead")]] std::uint32_t sum() const ...
                        ...
                        ... [[deprecated("Use AnotherStruct instead")]] std::uint32_t product() const ...
                        ...
                    };
                }
            )
        })
    }

    #[test]
    fn test_multiple_attributes() {
        let test_src = r#"
        #[must_use = "Must use"]
        #[deprecated = "Deprecated"]
        pub fn add(x: i32, y: i32) -> i32 {
            x + y
        }"#;

        test_format_item(test_src, "add", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    [[nodiscard("Must use")]] [[deprecated("Deprecated")]] std::int32_t add(std::int32_t x, std::int32_t y);
                        ...
                }
            )
        })
    }

    #[test]
    fn test_repr_c_union_fields() {
        let test_src = r#"
        #[repr(C)]
        pub union SomeUnion {
            pub x: u16,
            pub y: u32,
        }

        const _: () = assert!(std::mem::size_of::<SomeUnion>() == 4);
        const _: () = assert!(std::mem::align_of::<SomeUnion>() == 4);
        "#;

        test_format_item(test_src, "SomeUnion", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    union CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] SomeUnion final {
                        public:
                            ...
                            __COMMENT__ "`SomeUnion` doesn't implement the `Default` trait"
                            SomeUnion() = delete;
                            ...
                            __COMMENT__ "No custom `Drop` impl and no custom \"drop glue\" required"
                            ~SomeUnion() = default;
                            SomeUnion(SomeUnion&&) = default;
                            SomeUnion& operator=(SomeUnion&&) = default;

                            __COMMENT__ "`SomeUnion` doesn't implement the `Clone` trait"
                            SomeUnion(const SomeUnion&) = delete;
                            SomeUnion& operator=(const SomeUnion&) = delete;
                            ...
                            std::uint16_t x;
                            ...
                            std::uint32_t y;

                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeUnion) == 4, ...);
                    static_assert(alignof(SomeUnion) == 4, ...);
                    static_assert(std::is_trivially_destructible_v<SomeUnion>);
                    static_assert(std::is_trivially_move_constructible_v<SomeUnion>);
                    static_assert(std::is_trivially_move_assignable_v<SomeUnion>);
                    inline void SomeUnion::__crubit_field_offset_assertions() {
                      static_assert(0 == offsetof(SomeUnion, x));
                      static_assert(0 == offsetof(SomeUnion, y));
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeUnion>() == 4);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeUnion>() == 4);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeUnion, x) == 0);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeUnion, y) == 0);
                }
            );
        })
    }

    #[test]
    fn test_union_fields() {
        let test_src = r#"
        pub union SomeUnion {
            pub x: u16,
            pub y: u32,
        }

        const _: () = assert!(std::mem::size_of::<SomeUnion>() == 4);
        const _: () = assert!(std::mem::align_of::<SomeUnion>() == 4);
        "#;

        test_format_item(test_src, "SomeUnion", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    union CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] SomeUnion final {
                        public:
                            ...
                            __COMMENT__ "`SomeUnion` doesn't implement the `Default` trait"
                            SomeUnion() = delete;
                            ...
                            __COMMENT__ "No custom `Drop` impl and no custom \"drop glue\" required"
                            ~SomeUnion() = default;
                            SomeUnion(SomeUnion&&) = default;
                            SomeUnion& operator=(SomeUnion&&) = default;

                            __COMMENT__ "`SomeUnion` doesn't implement the `Clone` trait"
                            SomeUnion(const SomeUnion&) = delete;
                            SomeUnion& operator=(const SomeUnion&) = delete;
                            ...
                            struct {
                                ...
                                std::uint16_t value;
                            } x;
                            ...
                            struct {
                                ...
                                std::uint32_t value;
                            } y;
                        private:
                            static void __crubit_field_offset_assertions();
                    };
                }
            );

            // Note: we don't check for offsets here, because we don't know necessarily know
            // what the offset will be.
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeUnion) == 4, ...);
                    static_assert(alignof(SomeUnion) == 4, ...);
                    static_assert(std::is_trivially_destructible_v<SomeUnion>);
                    static_assert(std::is_trivially_move_constructible_v<SomeUnion>);
                    static_assert(std::is_trivially_move_assignable_v<SomeUnion>);
                    inline void SomeUnion::__crubit_field_offset_assertions() {
                      ...
                    }
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeUnion>() == 4);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeUnion>() == 4);
                    ...
                }
            );
        })
    }

    #[test]
    fn test_repr_c_union_unknown_fields() {
        let test_src = r#"
        #[repr(C)]
        pub union SomeUnion {
            pub z: std::mem::ManuallyDrop<i64>,
        }

        const _: () = assert!(std::mem::size_of::<SomeUnion>() == 8);
        const _: () = assert!(std::mem::align_of::<SomeUnion>() == 8);
        "#;

        test_format_item(test_src, "SomeUnion", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    union CRUBIT_INTERNAL_RUST_TYPE(...) alignas(8) [[clang::trivial_abi]] SomeUnion final {
                        public:
                            ...
                        private:
                            __COMMENT__ "Field type has been replaced with a blob of bytes: Generic types are not supported yet (b/259749095)"
                            unsigned char z[8];
                        ...
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    static_assert(sizeof(SomeUnion) == 8, ...);
                    static_assert(alignof(SomeUnion) == 8, ...);
                    ...
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    const _: () = assert!(::std::mem::size_of::<::rust_out::SomeUnion>() == 8);
                    const _: () = assert!(::std::mem::align_of::<::rust_out::SomeUnion>() == 8);
                    const _: () = assert!( ::core::mem::offset_of!(::rust_out::SomeUnion, z) == 0);
                }
            );
        })
    }

    #[test]
    fn test_repr_c_union_fields_impl_clone() {
        let test_src = r#"
        #[repr(C)]
        pub union SomeUnion {
            pub x: u32,
        }

        impl Clone for SomeUnion {
            fn clone(&self) -> SomeUnion {
                return SomeUnion {x: 1}
            }
        }

        const _: () = assert!(std::mem::size_of::<SomeUnion>() == 4);
        const _: () = assert!(std::mem::align_of::<SomeUnion>() == 4);
        "#;

        test_format_item(test_src, "SomeUnion", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    union CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] SomeUnion final {
                        public:
                            ...
                            __COMMENT__ "Clone::clone"
                            SomeUnion(const SomeUnion&);

                            __COMMENT__ "Clone::clone_from"
                            SomeUnion& operator=(const SomeUnion&);
                        ...
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    ...
                    static_assert(std::is_trivially_destructible_v<SomeUnion>);
                    static_assert(std::is_trivially_move_constructible_v<SomeUnion>);
                    static_assert(std::is_trivially_move_assignable_v<SomeUnion>);
                    ...
                    inline SomeUnion::SomeUnion(const SomeUnion& other) {...}
                    inline SomeUnion& SomeUnion::operator=(const SomeUnion& other) {...}
                    ...
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    ...
                    extern "C" fn ... (...) -> () {...(<::rust_out::SomeUnion as ::core::clone::Clone>::clone(__self...))...}
                    ...
                    extern "C" fn ... (...) -> () {...<::rust_out::SomeUnion as ::core::clone::Clone>::clone_from(__self, source)...}
                    ...
                }
            );
        })
    }

    #[test]
    fn test_repr_c_union_fields_impl_drop() {
        let test_src = r#"
        #[repr(C)]
        pub union SomeUnion {
            pub x: u32,
        }

        impl Drop for SomeUnion {
            fn drop(&mut self) {
                println!(":)")
            }
        }

        const _: () = assert!(std::mem::size_of::<SomeUnion>() == 4);
        const _: () = assert!(std::mem::align_of::<SomeUnion>() == 4);
        "#;

        test_format_item(test_src, "SomeUnion", |result| {
            let result = result.unwrap().unwrap();
            let main_api = &result.main_api;
            assert!(!main_api.prereqs.is_empty());
            assert_cc_matches!(
                main_api.tokens,
                quote! {
                    ...
                    union CRUBIT_INTERNAL_RUST_TYPE(...) alignas(4) [[clang::trivial_abi]] SomeUnion final {
                        public:
                            ...
                            __COMMENT__ "Drop::drop"
                            ~SomeUnion();

                            ...
                            SomeUnion(SomeUnion&&) = delete;
                            SomeUnion& operator=(SomeUnion&&) = delete;
                            ...
                        ...
                    };
                }
            );
            assert_cc_matches!(
                result.cc_details.tokens,
                quote! {
                    ...
                    inline SomeUnion::~SomeUnion() {...}
                    ...
                }
            );
            assert_rs_matches!(
                result.rs_details,
                quote! {
                    ...
                    extern "C" fn ... (__self: &mut ::core::mem::MaybeUninit<::rust_out::SomeUnion>...) { unsafe { __self.assume_init_drop() }; }
                    ...
                }
            );
        })
    }

    fn test_ty<TestFn, Expectation>(
        type_location: TypeLocation,
        testcases: &[(&str, Expectation)],
        preamble: TokenStream,
        test_fn: TestFn,
    ) where
        TestFn: for<'tcx> Fn(
                /* testcase_description: */ &str,
                TyCtxt<'tcx>,
                Ty<'tcx>,
                &Expectation,
            ) + Sync,
        Expectation: Sync,
    {
        for (index, (input, expected)) in testcases.iter().enumerate() {
            let desc = format!("test #{index}: test input: `{input}`");
            let input = {
                let ty_tokens: TokenStream = input.parse().unwrap();
                let input = match type_location {
                    TypeLocation::FnReturn => quote! {
                        #preamble
                        pub fn test_function() -> #ty_tokens { unimplemented!() }
                    },
                    TypeLocation::FnParam => quote! {
                        #preamble
                        pub fn test_function(_arg: #ty_tokens) { unimplemented!() }
                    },
                    TypeLocation::Other => unimplemented!(),
                };
                input.to_string()
            };
            run_compiler_for_testing(input, |tcx| {
                let def_id = find_def_id_by_name(tcx, "test_function");
                let sig = get_fn_sig(tcx, def_id);
                let ty = match type_location {
                    TypeLocation::FnReturn => sig.output(),
                    TypeLocation::FnParam => sig.inputs()[0],
                    TypeLocation::Other => unimplemented!(),
                };
                test_fn(&desc, tcx, ty, expected);
            });
        }
    }

    /// Tests invoking `format_item` on the item with the specified `name` from
    /// the given Rust `source`.  Returns the result of calling
    /// `test_function` with `format_item`'s result as an argument.
    /// (`test_function` should typically `assert!` that it got the expected
    /// result from `format_item`.)
    fn test_format_item<F, T>(source: &str, name: &str, test_function: F) -> T
    where
        F: FnOnce(Result<Option<ApiSnippets>, String>) -> T + Send,
        T: Send,
    {
        run_compiler_for_testing(source, |tcx| {
            let def_id = find_def_id_by_name(tcx, name);
            let result = bindings_db_for_tests(tcx).format_item(def_id);

            // https://docs.rs/anyhow/latest/anyhow/struct.Error.html#display-representations says:
            // To print causes as well [...], use the alternate selector “{:#}”.
            let result = result.map_err(|anyhow_err| format!("{anyhow_err:#}"));

            test_function(result)
        })
    }

    fn bindings_db_for_tests(tcx: TyCtxt) -> Database {
        Database::new(
            tcx,
            /* crubit_support_path_format= */ "<crubit/support/for/tests/{header}>".into(),
            /* crate_name_to_include_paths= */ Default::default(),
            /* errors = */ Rc::new(IgnoreErrors),
            /* _features= */ (),
        )
    }

    /// Tests invoking `generate_bindings` on the given Rust `source`.
    /// Returns the result of calling `test_function` with the generated
    /// bindings as an argument. (`test_function` should typically `assert!`
    /// that it got the expected `GeneratedBindings`.)
    fn test_generated_bindings<F, T>(source: &str, test_function: F) -> T
    where
        F: FnOnce(Result<Output>) -> T + Send,
        T: Send,
    {
        run_compiler_for_testing(source, |tcx| {
            test_function(generate_bindings(&bindings_db_for_tests(tcx)))
        })
    }
}
