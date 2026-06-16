// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use examples::object_gateway::{model::ObjectOperation, proto::SERVICE_NAME};

fn main() {
    println!(
        "service={SERVICE_NAME} operations={}",
        ObjectOperation::ALL.len()
    );
}
