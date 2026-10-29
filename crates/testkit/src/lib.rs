//! Test harness of the operator: the kit's envtest control plane with this
//! operator's CRDs, status polling, and the in-memory ports.

pub mod envtest;
pub mod polling;

pub use application::test_support::{
    FakeCredentials, FakeDirectory, FakeInstances, FakeLedger, FakeOwnedObjects,
};
