mod claude;
mod codex;

use std::future::Future;
use std::pin::Pin;

use crate::{
    AgentQuotaClient, ProbeFailure, ProviderCapability, ProviderId, ProviderProfile,
    ProviderUsageSnapshot,
};

pub(crate) type ProviderProbeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderUsageSnapshot, ProbeFailure>> + Send + 'a>>;

pub(crate) trait ProviderAdapter: Sync {
    fn provider_id(&self) -> ProviderId;
    fn capability(&self) -> ProviderCapability;
    fn collect<'a>(
        &'a self,
        client: &'a AgentQuotaClient,
        profile: &'a ProviderProfile,
    ) -> ProviderProbeFuture<'a>;
}

static CODEX: codex::CodexAdapter = codex::CodexAdapter;
static CLAUDE: claude::ClaudeAdapter = claude::ClaudeAdapter;

pub(crate) fn adapter(provider: ProviderId) -> &'static dyn ProviderAdapter {
    match provider {
        ProviderId::Codex => &CODEX,
        ProviderId::Claude => &CLAUDE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_collection_futures_remain_send() {
        fn assert_send<T: Send>(_: T) {}

        let client = AgentQuotaClient::new();
        let profile = ProviderProfile::default_for_provider(ProviderId::Codex);
        assert_send(client.collect_profile_usage(profile));
    }
    #[test]
    fn every_provider_resolves_to_a_matching_adapter() {
        for provider in [ProviderId::Codex, ProviderId::Claude] {
            let adapter = adapter(provider);
            assert_eq!(adapter.provider_id(), provider);
            assert_eq!(adapter.capability().provider_id, provider);
        }
    }
}
