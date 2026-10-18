# Security policy

SlopSearch is experimental software. It has no authentication, no TLS and no access
control, and it keeps all data in memory. Do not expose it to untrusted networks or store
sensitive data in it. Reports that it lacks these features are known limitations, not
vulnerabilities.

Things we do treat as security issues: a request that crashes the server, makes it
consume unbounded memory or CPU, or reads or writes outside its own data.

Please report them privately through GitHub's
[private vulnerability reporting](../../security/advisories/new) instead of a public issue.
Only the latest commit on `main` is supported.
