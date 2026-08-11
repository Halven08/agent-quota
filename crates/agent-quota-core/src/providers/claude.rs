use crate::{
    AgentQuotaClient, ProviderCapability, ProviderCredentialSource, ProviderId,
    ProviderProbeTransport, ProviderProfile, ProviderQuotaSource, DEFAULT_CACHE_TTL_SECONDS,
};

use super::{ProviderAdapter, ProviderProbeFuture};

pub(crate) struct ClaudeAdapter;

impl ProviderAdapter for ClaudeAdapter {
    fn provider_id(&self) -> ProviderId {
        ProviderId::Claude
    }

    fn capability(&self) -> ProviderCapability {
        let provider = self.provider_id();
        ProviderCapability {
            provider_id: provider,
            provider_name: provider.label().to_owned(),
            probe_transport: ProviderProbeTransport::RemoteApi,
            quota_source: ProviderQuotaSource::AnthropicRateLimitHeaders,
            credential_source: ProviderCredentialSource::ClaudeCodeOauthFile,
            submits_message: true,
            may_affect_quota_or_billing: true,
            default_cache_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
            probe_impact:
                "Sends a fixed one-token `hi` message to Anthropic; this may affect quota or billing."
                    .to_owned(),
        }
    }

    fn collect<'a>(
        &'a self,
        client: &'a AgentQuotaClient,
        profile: &'a ProviderProfile,
    ) -> ProviderProbeFuture<'a> {
        Box::pin(async move { client.query_claude_usage(profile).await })
    }
}
