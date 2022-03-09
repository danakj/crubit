# Part of the Crubit project, under the Apache License v2.0 with LLVM
# Exceptions. See /LICENSE for license information.
# SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

"""A transition that helps us build the rs_bindings_from_cc tool."""

def _rust_bindings_from_cc_aspect_impl(settings, attr):
    return {
        "//rs_bindings_from_cc/bazel_support:use_actual_bindings_generator": False,
    }

rust_bindings_from_cc_transition = transition(
    implementation = _rust_bindings_from_cc_aspect_impl,
    inputs = [],
    outputs = ["//rs_bindings_from_cc/bazel_support:use_actual_bindings_generator"],
)
