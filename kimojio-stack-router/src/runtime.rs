// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime-family type aliases used by router adapters and examples.

/// Marker for `kimojio-stack` router execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StackRuntimeFamily;

/// Marker for `kimojio-stack-steal` router execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StealRuntimeFamily;

/// `kimojio-stack` runtime context type accepted by router services.
pub type StackContext<'cx> = kimojio_stack::RuntimeContext<'cx>;

/// `kimojio-stack-steal` runtime context type accepted by router services.
pub type StealContext<'cx> = kimojio_stack_steal::RuntimeContext<'cx>;
