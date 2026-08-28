//! Cloud provider abstraction for flyco.
//!
//! Every implementation is an HTTP client over the provider's public API
//! (signed requests via [`zenwave`]), never a native SDK — the same code
//! runs in the Cloudflare Worker (wasm32) and in tests. Implementations
//! land per milestone: `byo-ssh`, then Azure, then AWS and GCP.

use flyco_core::MachineId;
use flyco_core::machine::{MachineCatalogEntry, MachineSpec, MachineState};

/// A provisioned machine as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    /// Flyco's identifier for the machine.
    pub id: MachineId,
    /// Provider-native resource identifier (instance id, VM resource id,
    /// or container id for byo-ssh).
    pub native_id: String,
    /// Current lifecycle state.
    pub state: MachineState,
    /// Address the daemon bootstrap reaches it on, once known.
    pub address: Option<String>,
}

/// An error from a provider operation.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The provider's API rejected the request.
    #[error("provider rejected the request: {0}")]
    Rejected(String),
    /// The provider has no capacity for the requested spec.
    #[error("no capacity for the requested machine type: {0}")]
    NoCapacity(String),
    /// Transport-level failure talking to the provider.
    #[error("transport error: {0}")]
    Transport(#[from] zenwave::Error),
}

/// A compute provider flyco can provision session machines on.
///
/// Object-unsafe by design: the control plane matches on
/// [`flyco_core::machine::CloudProviderKind`] and calls the concrete
/// implementation, keeping every future free of boxing on wasm32.
pub trait CloudProvider {
    /// The machine types this provider currently offers, with live pricing.
    fn catalog(&self) -> impl Future<Output = Result<Vec<MachineCatalogEntry>, ProviderError>>;

    /// Provisions a machine for a session.
    fn provision(&self, spec: &MachineSpec)
    -> impl Future<Output = Result<Machine, ProviderError>>;

    /// Changes the machine type in place, preserving the disk
    /// (stop → modify → start).
    fn resize(
        &self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> impl Future<Output = Result<Machine, ProviderError>>;

    /// Releases compute but keeps the disk (archive-pending, spot pause).
    fn deallocate(&self, machine: &Machine) -> impl Future<Output = Result<(), ProviderError>>;

    /// Releases compute and disk. Irreversible.
    fn destroy(&self, machine: &Machine) -> impl Future<Output = Result<(), ProviderError>>;
}
