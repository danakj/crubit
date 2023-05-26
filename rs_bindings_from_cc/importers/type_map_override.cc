// Part of the Crubit project, under the Apache License v2.0 with LLVM
// Exceptions. See /LICENSE for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include "rs_bindings_from_cc/importers/type_map_override.h"

#include <optional>
#include <string>
#include <utility>

#include "absl/status/status.h"
#include "common/status_macros.h"
#include "rs_bindings_from_cc/ir.h"
#include "clang/AST/ASTContext.h"
#include "clang/AST/Attr.h"
#include "clang/AST/Attrs.inc"
#include "clang/AST/Decl.h"
#include "clang/AST/Type.h"

namespace crubit {
namespace {

// Copied from lifetime_annotations/type_lifetimes.cc, which is expected to move
// into ClangTidy. See:
// https://discourse.llvm.org/t/rfc-lifetime-annotations-for-c/61377
absl::StatusOr<absl::string_view> EvaluateAsStringLiteral(
    const clang::Expr& expr, const clang::ASTContext& ast_context) {
  auto error = []() {
    return absl::InvalidArgumentError(
        "cannot evaluate argument as a string literal");
  };

  clang::Expr::EvalResult eval_result;
  if (!expr.EvaluateAsConstantExpr(eval_result, ast_context) ||
      !eval_result.Val.isLValue()) {
    return error();
  }

  const auto* eval_result_expr =
      eval_result.Val.getLValueBase().dyn_cast<const clang::Expr*>();
  if (!eval_result_expr) {
    return error();
  }

  const auto* string_literal =
      clang::dyn_cast<clang::StringLiteral>(eval_result_expr);
  if (!string_literal) {
    return error();
  }

  return {string_literal->getString()};
}

absl::StatusOr<std::optional<absl::string_view>> GetRustTypeAttribute(
    const clang::Type& cc_type) {
  std::optional<absl::string_view> rust_type;
  const clang::Decl* decl = nullptr;
  if (const auto* alias_type = cc_type.getAs<clang::TypedefType>();
      alias_type != nullptr) {
    decl = alias_type->getDecl();
  } else if (const clang::TagDecl* tag_decl = cc_type.getAsTagDecl();
             tag_decl != nullptr) {
    decl = tag_decl;
  }
  if (decl != nullptr) {
    for (clang::AnnotateAttr* attr :
         decl->specific_attrs<clang::AnnotateAttr>()) {
      if (attr->getAnnotation() != "crubit_internal_rust_type") continue;

      if (rust_type.has_value())
        return absl::InvalidArgumentError(
            "Only one `crubit_internal_rust_type` attribute may be placed on a "
            "type.");
      if (attr->args_size() != 1)
        return absl::InvalidArgumentError(
            "The `crubit_internal_rust_type` attribute requires a single "
            "string literal "
            "argument, the Rust type.");
      const clang::Expr& arg = **attr->args_begin();
      CRUBIT_ASSIGN_OR_RETURN(
          rust_type, EvaluateAsStringLiteral(arg, decl->getASTContext()));
    }
  }
  return rust_type;
}
}  // namespace

std::optional<IR::Item> TypeMapOverrideImporter::Import(
    clang::TypeDecl* type_decl) {
  clang::ASTContext& context = type_decl->getASTContext();
  clang::QualType cc_qualtype = context.getTypeDeclType(type_decl);
  const clang::Type* cc_type = cc_qualtype.getTypePtr();
  if (cc_type == nullptr) return std::nullopt;

  absl::StatusOr<std::optional<absl::string_view>> rust_type =
      GetRustTypeAttribute(*cc_type);
  if (!rust_type.ok()) {
    return ictx_.ImportUnsupportedItem(
        type_decl, absl::StrCat("Invalid crubit_internal_rust_type attribute: ",
                                rust_type.status().message()));
  }
  if (!rust_type->has_value()) {
    return std::nullopt;
  }
  auto rs_name = std::string(**rust_type);
  std::string cc_name = cc_qualtype.getAsString();

  ictx_.MarkAsSuccessfullyImported(type_decl);

  std::optional<SizeAlign> size_align;
  if (!cc_type->isIncompleteType()) {
    size_align = SizeAlign{
        .size = context.getTypeSizeInChars(cc_type).getQuantity(),
        .alignment = context.getTypeAlignInChars(cc_type).getQuantity(),
    };
  }
  return TypeMapOverride{
      .rs_name = std::move(rs_name),
      .cc_name = std::move(cc_name),
      .owning_target = ictx_.GetOwningTarget(type_decl),
      .size_align = std::move(size_align),
      .id = GenerateItemId(type_decl),
  };
}

}  // namespace crubit
