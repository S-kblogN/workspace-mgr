# Security policy

Please report a suspected vulnerability privately through GitHub's security
advisory feature for this repository. Do not open a public issue containing
credentials, exploit details, or sensitive repository data.

`workspace-mgr` executes Git and optional DVC commands with the caller's local
permissions. Repository configuration is treated as repository-controlled
input, while credentials must remain in tool-specific local configuration or
the process environment and are never copied into generated manifests.
