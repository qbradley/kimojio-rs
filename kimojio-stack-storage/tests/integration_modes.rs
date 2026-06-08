// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use kimojio_stack_storage::{AuthMode, CompatibilityFixtures, ContainerName, StorageContext};

#[test]
fn caller_environment_modes_remain_represented() {
    let fixtures = CompatibilityFixtures::built_in();
    let names = fixtures
        .caller_environments
        .iter()
        .map(|environment| environment.name.as_str())
        .collect::<Vec<_>>();

    assert!(names.contains(&"page-writer"));
    assert!(names.contains(&"archive-reader"));
    assert!(
        fixtures
            .caller_environments
            .iter()
            .any(|environment| environment.requires_async_bridge)
    );
}

#[test]
fn storage_context_supports_default_and_emulator_modes() {
    let default = StorageContext::new(
        "https://example.invalid",
        ContainerName::new("container"),
        AuthMode::Identity { client_id: None },
    );
    let emulator = StorageContext::new(
        "http://127.0.0.1/devstore",
        ContainerName::new("container"),
        AuthMode::Emulator {
            account: "devstore".into(),
            secure: false,
        },
    )
    .with_emulator(true);

    assert!(!default.emulator);
    assert!(emulator.emulator);
}
