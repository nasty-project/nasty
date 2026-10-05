# TLS DNS-01 providers

DNS-01 issues certificates by creating DNS TXT records; it does not require
inbound ports 80 or 443. The appliance still needs outbound access to the ACME
service, the provider API, and DNS resolvers.

## Aliyun (AliDNS)

In **Protection → TLS**, enable Let's Encrypt, select the **DNS** challenge,
then select **Aliyun (AliDNS)**. The provider code is `alidns`, not `aliyun`.
Your domain's authoritative DNS must be hosted by AliDNS.

Enter credentials as one `KEY=VALUE` per line:

```text
ALIYUN_ACCESS_KEY_ID=your-access-key-id
ALIYUN_ACCESS_KEY_SECRET=your-access-key-secret
```

For temporary STS credentials, also provide `ALIYUN_SECURITY_TOKEN`. Temporary
credentials must remain valid for issuance and subsequent renewals; NASty does
not automatically refresh them.

Use a dedicated RAM identity rather than a primary-account access key. The
[upstream provider](https://github.com/libdns/alidns/tree/v1.0.7#authenticating)
requires these AliDNS API actions:

- `alidns:DescribeDomains`
- `alidns:DescribeDomainRecords`
- `alidns:AddDomainRecord`
- `alidns:UpdateDomainRecord`
- `alidns:DeleteDomainRecord`

Scope permissions to the required DNS resources where Aliyun supports it.
NASty stores credentials encrypted at rest and passes them to Caddy through
its credential environment file, not as literal secrets in the generated JSON.

Use the staging option for an initial issuance test, then disable staging to
request a publicly trusted certificate. If propagation checks fail, configure
reachable resolvers and a longer propagation wait under **Advanced DNS settings**.

The bundled Caddy plugin is `github.com/caddy-dns/alidns@v1.0.29` and registers
`dns.providers.alidns`. Tencent Cloud and West.cn are not added by this change.
