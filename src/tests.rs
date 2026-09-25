// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Cross-module tests, plus (in this directory) each module's own unit tests, loaded via
//! `#[path]` from its own tests file or `tests/<module>/mod.rs`. Those files are not submodules
//! of `crate::tests`, just neighbors of it on disk.

mod scenarios;
