# Security Policy

`copycat` handles sensitive clipboard contents (passwords, API tokens, private keys) in memory. Security disclosures are taken seriously.

## Reporting a Vulnerability

**Do not open public issues for security vulnerabilities.**

Please use GitHub's private vulnerability reporting feature:
1. Navigate to the **Security** tab of this repository.
2. Click **Report a vulnerability**.

When submitting a report, please include:
- Repository version/commit SHA, OS, and launch configuration.
- Steps to reproduce or a test case.
- Impact assessment (e.g., memory exposure, DoS).

## Supported Versions

| Version | Supported |
|---|---|
| `main` | Yes |
| Latest release | Yes |
| Older releases | No |

## Security Scope

### In Scope
- **IPC Channel Security**: Permissions on unix sockets (`0600`/`0700`) and Windows named-pipe DACLs.
- **Masking Policy Enforcement**: Preventing `Secret` items from reaching model outputs in any privacy posture.
- **Memory Zeroization & Logging**: Ensuring credentials/secrets are zeroized on drop and never emitted in application logs.
- **IPC Denial of Service**: Panics or unbounded allocations reachable over the wire.

### Out of Scope / Design Decisions
- **Same-User Process Access**: Processes running as the same user can access the socket by design.
- **Heuristic Classifier Misses**: `pii::classify` operates on best-effort pattern matching.
- **Debug Builds**: Unsafe reveal paths explicitly enabled via `--features debug-unsafe` or `COPYCAT_INSECURE`.

## Security Recommendations

- Set `COPYCAT_PRIVACY=strict` when using third-party/hosted model endpoints.
- Store sockets only in secure, restricted directories (`0700`).
- Do not enable `debug-unsafe` features in production builds.
