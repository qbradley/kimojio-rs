// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{AccountId, ObjectKind, ObjectRef};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountEndpoint {
    pub account: AccountId,
    pub endpoint: String,
}

impl AccountEndpoint {
    pub fn new(account: AccountId, endpoint: impl Into<String>) -> Self {
        Self {
            account,
            endpoint: endpoint.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutedObject {
    pub account: AccountId,
    pub endpoint: String,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutingTable {
    primary: AccountEndpoint,
    data_accounts: Vec<AccountEndpoint>,
}

impl RoutingTable {
    pub fn new(primary: AccountEndpoint) -> Self {
        Self {
            primary,
            data_accounts: Vec::new(),
        }
    }

    pub fn with_data_accounts(mut self, data_accounts: Vec<AccountEndpoint>) -> Self {
        self.data_accounts = data_accounts;
        self
    }

    pub fn route(&self, object: &ObjectRef) -> RoutedObject {
        let endpoint = match object.kind {
            ObjectKind::Data if !self.data_accounts.is_empty() => {
                let index = stable_hash(object.name.as_str().as_bytes()) as usize
                    % self.data_accounts.len();
                &self.data_accounts[index]
            }
            _ => &self.primary,
        };

        RoutedObject {
            account: endpoint.account.clone(),
            endpoint: endpoint.endpoint.clone(),
            path: format!(
                "{}/{}/{}",
                endpoint.account.as_str(),
                object.container.as_str(),
                object.name.as_str()
            ),
        }
    }
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use crate::{CompatibilityFixtures, ContainerName, ObjectName};

    use super::*;

    fn object(name: &str, kind: ObjectKind) -> ObjectRef {
        ObjectRef {
            account: AccountId::new("ignored"),
            container: ContainerName::new("container"),
            name: ObjectName::new(name),
            kind,
        }
    }

    #[test]
    fn routing_uses_primary_for_metadata_and_stable_data_account_for_data() {
        let routing = RoutingTable::new(AccountEndpoint::new(AccountId::new("primary"), "p"))
            .with_data_accounts(vec![
                AccountEndpoint::new(AccountId::new("data0"), "d0"),
                AccountEndpoint::new(AccountId::new("data1"), "d1"),
            ]);

        let metadata = routing.route(&object("metadata/root", ObjectKind::Metadata));
        let data_a = routing.route(&object("rel/1/page", ObjectKind::Data));
        let data_a_again = routing.route(&object("rel/1/page", ObjectKind::Data));
        let data_b = routing.route(&object("rel/2/page", ObjectKind::Data));

        assert_eq!(metadata.account.as_str(), "primary");
        assert_eq!(data_a, data_a_again);
        assert!(matches!(data_a.account.as_str(), "data0" | "data1"));
        assert!(matches!(data_b.account.as_str(), "data0" | "data1"));
        assert!(data_a.path.ends_with("/container/rel/1/page"));
    }

    #[test]
    fn routing_matches_built_in_compatibility_fixtures() {
        let accounts = [
            AccountEndpoint::new(AccountId::new("primary"), "p"),
            AccountEndpoint::new(AccountId::new("data0"), "d0"),
        ];
        let routing =
            RoutingTable::new(accounts[0].clone()).with_data_accounts(vec![accounts[1].clone()]);

        for fixture in CompatibilityFixtures::built_in().routing {
            let routed = routing.route(&object(&fixture.object_name, fixture.kind));
            assert_eq!(
                routed.account,
                accounts[fixture.expected_account_index].account
            );
        }
    }
}
