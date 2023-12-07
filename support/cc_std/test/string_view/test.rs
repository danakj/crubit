// Part of the Crubit project, under the Apache License v2.0 with LLVM
// Exceptions. See /LICENSE for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use cc_std::*;
use string_view_apis::crubit_string_view::GetHelloWorld;

/// Converts a string_view to a &'static str.
///
/// SAFETY: Behavior is undefined if the string_view has an invalid pointer,
/// or points to data with non-static lifetime.
unsafe fn to_str(sv: std::string_view) -> &'static str {
    let bytes: &'static [u8] = unsafe { &*<*const [u8]>::from(sv) };
    core::str::from_utf8(bytes).unwrap()
}

#[test]
fn test_round_trip_empty_slice() {
    let original: &'static [u8] = &[];
    let sv: std::string_view = original.into();
    let raw_round_tripped = <*const [u8]>::from(sv);
    assert_ne!(raw_round_tripped, original as *const _); // dangling -> null -> new dangling
    assert_eq!(unsafe { &*raw_round_tripped }, original);
}

#[test]
fn test_round_trip_str() {
    let original: &'static str = "this is a string";
    let sv: std::string_view = original.into();
    assert_eq!(unsafe { to_str(sv) }, original);
}

#[test]
fn test_ffi() {
    assert_eq!(unsafe { to_str(GetHelloWorld()) }, "Hello, world!");
}
