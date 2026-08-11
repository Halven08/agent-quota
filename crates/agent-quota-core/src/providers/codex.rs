use crate::{
    AgentQuotaClient, ProviderCapability, ProviderCredentialSource, ProviderId,
    ProviderProbeTransport, ProviderProfile, ProviderQuotaSource, DEFAULT_CACHE_TTL_SECONDS,
};

use super::{ProviderAdapter, ProviderProbeFuture};

pub(crate) struct CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn provider_id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn capability(&self) -> ProviderCapability {
        let provider = self.provider_id();
        ProviderCapability {
            provider_id: provider,
            provider_name: provider.label().to_owned(),
            probe_transport: ProviderProbeTransport::LocalProcess,
            quota_source: ProviderQuotaSource::CodexAppServer,
            credential_source: ProviderCredentialSource::CodexCliSession,
            submits_message: false,
            may_affect_quota_or_billing: false,
            default_cache_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
            probe_impact:
                "Starts the local Codex app-server and reads account rate limits without submitting a prompt."
                    .to_owned(),
        }
    }

    fn collect<'a>(
        &'a self,
        client: &'a AgentQuotaClient,
        profile: &'a ProviderProfile,
    ) -> ProviderProbeFuture<'a> {
        Box::pin(async move { client.query_codex_usage(profile).await })
    }
}
