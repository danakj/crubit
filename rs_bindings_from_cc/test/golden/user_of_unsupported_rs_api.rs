// Part of the Crubit project, under the Apache License v2.0 with LLVM
// Exceptions. See /LICENSE for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

// Automatically @generated Rust bindings for the following C++ target:
// //rs_bindings_from_cc/test/golden:user_of_unsupported_cc
// Features: experimental, supported

#![rustfmt::skip]
#![feature(custom_inner_attributes, impl_trait_in_assoc_type)]
#![allow(stable_features)]
#![no_std]
#![allow(improper_ctypes)]
#![allow(nonstandard_style)]
#![deny(warnings)]

#[inline(always)]
pub fn UseNontrivialCustomType(
    non_trivial_custom_type: impl ::ctor::Ctor<Output = unsupported_cc::NontrivialCustomType>,
) {
    unsafe {
        crate::detail::__rust_thunk___Z23UseNontrivialCustomType20NontrivialCustomType(
            ::core::pin::Pin::into_inner_unchecked(::ctor::emplace!(non_trivial_custom_type)),
        )
    }
}

mod detail {
    #[allow(unused_imports)]
    use super::*;
    extern "C" {
        pub(crate) fn __rust_thunk___Z23UseNontrivialCustomType20NontrivialCustomType(
            non_trivial_custom_type: &mut unsupported_cc::NontrivialCustomType,
        );
    }
}
