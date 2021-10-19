// Part of the Crubit project, under the Apache License v2.0 with LLVM
// Exceptions. See /LICENSE for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <cstddef>
#include "rs_bindings_from_cc/test/golden/unsupported.h"

static_assert(sizeof(NontrivialCustomType) == 4);
static_assert(alignof(NontrivialCustomType) == 4);
static_assert(offsetof(NontrivialCustomType, i) * 8 == 0);

static_assert(sizeof(ContainingStruct) == 4);
static_assert(alignof(ContainingStruct) == 4);
static_assert(offsetof(ContainingStruct, placeholder) * 8 == 0);
