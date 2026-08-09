# Security policy

Please do not disclose suspected vulnerabilities in a public issue.

Use GitHub's **Report a vulnerability** form in the Security tab of this
repository. Include the affected release or commit, operating system,
reproduction steps, impact, and whether real data may have been exposed. Never
attach credentials, tokens, private keys, or unredacted memory content.

Security fixes are provided for the latest Community release and current
`main`. Solo's supported threat model is one user on a trusted local machine;
the HTTP server defaults to loopback and must not be exposed directly to the
public internet.

Solo has not completed an independent security audit and must not be presented
as certified for regulated or high-risk data.
