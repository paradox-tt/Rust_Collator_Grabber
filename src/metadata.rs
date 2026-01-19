//! Typed metadata modules for each chain.
//! Generated from .scale files using subxt's #[subxt] macro.

#[subxt::subxt(runtime_metadata_path = "metadata/asset-hub-polkadot.scale")]
pub mod asset_hub_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/asset-hub-kusama.scale")]
pub mod asset_hub_kusama {}

#[subxt::subxt(runtime_metadata_path = "metadata/bridge-hub-polkadot.scale")]
pub mod bridge_hub_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/bridge-hub-kusama.scale")]
pub mod bridge_hub_kusama {}

#[subxt::subxt(runtime_metadata_path = "metadata/collectives-polkadot.scale")]
pub mod collectives_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/coretime-polkadot.scale")]
pub mod coretime_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/coretime-kusama.scale")]
pub mod coretime_kusama {}

#[subxt::subxt(runtime_metadata_path = "metadata/people-polkadot.scale")]
pub mod people_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/people-kusama.scale")]
pub mod people_kusama {}

#[subxt::subxt(runtime_metadata_path = "metadata/encointer-kusama.scale")]
pub mod encointer_kusama {}
