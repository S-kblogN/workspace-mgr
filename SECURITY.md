# Security policy

Please report a suspected vulnerability privately through GitHub's security
advisory feature for this repository. Do not open a public issue containing
credentials, exploit details, or sensitive repository data.

`workspace-mgr` executes the platform Git client and its private storage runtime
with the caller's local permissions. Repository configuration is treated as
repository-controlled input. Credentials must remain in ignored local
configuration or platform-standard identity and environment mechanisms; they
are never copied into generated policy, manifests, or reports.
