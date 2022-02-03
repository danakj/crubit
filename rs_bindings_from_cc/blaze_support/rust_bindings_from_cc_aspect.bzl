# Part of the Crubit project, under the Apache License v2.0 with LLVM
# Exceptions. See /LICENSE for license information.
# SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

    "//rs_bindings_from_cc/bazel_support:rust_bindings_from_cc_binary.bzl",
    "GeneratedFilesDepsInfo",
)
load(
    "//rs_bindings_from_cc/bazel_support:rust_bindings_from_cc_utils.bzl",
    "RustBindingsFromCcInfo",
    "bindings_attrs",
    "generate_and_compile_bindings",
)

# buildifier: disable=bzl-visibility
load("//third_party/bazel_rules/rules_rust/rust/private:providers.bzl", "DepVariantInfo")

# <internal link>/127#naming-header-files-h-and-inc recommends declaring textual headers either in the
# `textual_hdrs` attribute of the Blaze C++ rules, or using the `.inc` file extension. Therefore
# we are omitting ["inc"] from the list below.
_hdr_extensions = ["h", "hh", "hpp", "ipp", "hxx", "h++", "inl", "tlh", "tli", "H", "tcc"]

def _filter_none(input_list):
    return [element for element in input_list if element != None]

def _is_hdr(input):
    return input.path.split(".")[-1] in _hdr_extensions

def _filter_hdrs(input_list):
    return [hdr for hdr in input_list if _is_hdr(hdr)]

public_headers_to_remove = {
    "//base:base": [
        "base/callback.h",  # //base:callback
        "base/callback-specializations.h",  # //base:callback
        "base/callback-types.h",  # //base:callback
        "base/file_toc.h",  # //base:file_toc
        "base/googleinit.h",  # //base:googleinit
        "base/logging.h",  # //base:logging
    ],
}

def _collect_hdrs(ctx):
    public_hdrs = _filter_hdrs(ctx.rule.files.hdrs)
    private_hdrs = _filter_hdrs(ctx.rule.files.srcs) if hasattr(ctx.rule.attr, "srcs") else []
    label = str(ctx.label)
    public_hdrs = [
        h
        for h in public_hdrs
        if h.short_path not in public_headers_to_remove.get(label, [])
    ]

    all_standalone_hdrs = public_hdrs + private_hdrs
    return public_hdrs, all_standalone_hdrs

def _rust_bindings_from_cc_aspect_impl(target, ctx):
    # We use a fake generator only when we are building the real one, in order to avoid
    # dependency cycles.
    if ctx.executable._generator.basename == "fake_rust_bindings_from_cc":
        return []

    # If this target already provides bindings, we don't need to run the bindings generator.
    if RustBindingsFromCcInfo in target:
        return []

    # This is not a C++ rule
    if CcInfo not in target:
        return []

    if not hasattr(ctx.rule.attr, "hdrs"):
        return []

    public_hdrs, all_standalone_hdrs = _collect_hdrs(ctx)

    # At execution time we convert this depset to a json array that gets passed to our tool through
    # the --targets_and_headers flag.
    # We can improve upon this solution if:
    # 1. we use a library for parsing command line flags that allows repeated flags.
    # 2. instead of json string, we use a struct that will be expanded to flags at execution time.
    #    This requires changes to Blaze.
    targets_and_headers = depset(
        direct = [
            json.encode({
                "t": str(ctx.label),
                "h": [h.path for h in all_standalone_hdrs],
            }),
        ] if all_standalone_hdrs else [],
        transitive = [
            t[RustBindingsFromCcInfo].targets_and_headers
            for t in ctx.rule.attr.deps
            if RustBindingsFromCcInfo in t
        ] + [
            # TODO(b/217667751): This is a huge list of headers; pass it as a file instead;
            ctx.attr._std[RustBindingsFromCcInfo].targets_and_headers,
        ],
    )

    if not public_hdrs:
        empty_cc_info = CcInfo()
        return RustBindingsFromCcInfo(
            cc_info = empty_cc_info,
            dep_variant_info = DepVariantInfo(cc_info = empty_cc_info),
            targets_and_headers = targets_and_headers,
        )

    header_includes = []
    for hdr in public_hdrs:
        header_includes.append("-include")
        header_includes.append(hdr.short_path)

    stl = ctx.attr._stl[CcInfo].compilation_context
    compilation_context = target[CcInfo].compilation_context

    return generate_and_compile_bindings(
        ctx,
        ctx.rule.attr,
        compilation_context = compilation_context,
        public_hdrs = public_hdrs,
        header_includes = header_includes,
        action_inputs = public_hdrs + ctx.files._builtin_hdrs,
        targets_and_headers = targets_and_headers,
        deps_for_cc_file = [target[CcInfo]] + [
            dep[RustBindingsFromCcInfo].cc_info
            for dep in ctx.rule.attr.deps
            if RustBindingsFromCcInfo in dep
        ] + ctx.attr._generator[GeneratedFilesDepsInfo].deps_for_cc_file + [
            ctx.attr._std[RustBindingsFromCcInfo].cc_info,
        ],
        deps_for_rs_file = [
            dep[RustBindingsFromCcInfo].dep_variant_info
            for dep in ctx.rule.attr.deps
            if RustBindingsFromCcInfo in dep
        ] + ctx.attr._generator[GeneratedFilesDepsInfo].deps_for_rs_file + [
            ctx.attr._std[RustBindingsFromCcInfo].dep_variant_info,
        ],
    )

rust_bindings_from_cc_aspect = aspect(
    implementation = _rust_bindings_from_cc_aspect_impl,
    attr_aspects = ["deps"],
    attrs = dict(bindings_attrs.items() + {
        "_std": attr.label(
            default = "//rs_bindings_from_cc:cc_std",
        ),
    }.items()),
    toolchains = [
        "//third_party/bazel_rules/rules_rust/rust:toolchain",
        "//tools/cpp:toolchain_type",
    ],
    host_fragments = ["cpp"],
    fragments = ["cpp", "google_cpp"],
)
