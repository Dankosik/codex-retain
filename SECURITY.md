# Security policy

Security fixes target the current development branch and latest release. There
is no promised backport or response-time commitment. Codex Retain supports only
the explicitly reviewed Codex storage and writer versions in its documentation.

## Reporting a vulnerability

Use **Security → Advisories → Report a vulnerability** on this repository when
GitHub private vulnerability reporting is enabled. Include the affected commit
or version, platform, reproduction steps, expected trust boundary, and observed
impact. Use synthetic inputs and redact credentials or personal data.

If private reporting is unavailable, open an issue requesting a private
reporting channel without exploit details, secrets, or sensitive attachments.
Do not publish an exploit in a normal bug report while seeking that channel.

## Scope

Relevant issues include unsafe handling of untrusted input or paths, unintended
file or subprocess effects, secret disclosure, memory-safety defects, and release
or dependency supply-chain problems. A crash, resource spike, or confusing error
may be a regular bug; describe the input and impact so it can be assessed.

The utility has no hosted service or production credentials to test. Never use
real conversations for destructive reproductions. Do not scan third-party
systems or include their private data in a reproduction.

Keep dependency and workflow updates reviewable, and publish artifacts only from
the candidate covered by the release checks. The initial local source delivery
has no published release or active private-reporting channel configured by this
development session; maintainers must configure those before public release.
