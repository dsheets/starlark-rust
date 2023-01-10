/*
 * Copyright 2018 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::cell::RefMut;
use std::ops::Deref;
use std::ops::DerefMut;

use gazebo::cell::ARef;

use crate::values::dict::Dict;
use crate::values::type_repr::StarlarkTypeRepr;
use crate::values::UnpackValue;
use crate::values::Value;

/// Borrowed `Dict`.
pub struct DictRef<'v> {
    pub(crate) aref: ARef<'v, Dict<'v>>,
}

/// Mutably borrowed `Dict`.
pub struct DictMut<'v> {
    pub(crate) aref: RefMut<'v, Dict<'v>>,
}

impl<'v> Deref for DictRef<'v> {
    type Target = Dict<'v>;

    fn deref(&self) -> &Self::Target {
        &self.aref
    }
}

impl<'v> Deref for DictMut<'v> {
    type Target = Dict<'v>;

    fn deref(&self) -> &Self::Target {
        &self.aref
    }
}

impl<'v> DerefMut for DictMut<'v> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.aref
    }
}

impl<'v> StarlarkTypeRepr for DictRef<'v> {
    fn starlark_type_repr() -> String {
        Dict::<'v>::starlark_type_repr()
    }
}

impl<'v> UnpackValue<'v> for DictRef<'v> {
    fn expected() -> String {
        "dict".to_owned()
    }

    fn unpack_value(value: Value<'v>) -> Option<DictRef<'v>> {
        Dict::from_value(value)
    }
}
