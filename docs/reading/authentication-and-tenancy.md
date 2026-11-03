<!-- Author: Nicola Antonello -->

# Authentication and tenant isolation

API keys, object-level authorization and row-level security. Part of the [reading list](README.md).

- ★ OWASP, [API1:2023 Broken Object Level Authorization](https://api-security.owasp.org/editions/2023/en/0xa1-broken-object-level-authorization) (OWASP API Security Top 10, 2023). The top API risk: an endpoint trusts an object's ID from the client, so one forgotten check exposes another customer's data.
- ★ PostgreSQL, [5.9. Row Security Policies](https://www.postgresql.org/docs/18/ddl-rowsecurity.html) (PostgreSQL 18 docs). Default deny, who bypasses a policy, why foreign-key checks always do, and the covert channels and races to watch for.
- AWS, [Row-level security recommendations](https://docs.aws.amazon.com/prescriptive-guidance/latest/saas-multitenant-managed-postgresql/rls.html) (AWS Prescriptive Guidance). Row-level security as the way to keep tenants apart when they share one database and one schema.
- NIST, [SP 800-63B-4: Digital Identity Guidelines: Authentication and Authenticator Management](https://pages.nist.gov/800-63-4/sp800-63b.html) (2025). Section 3.1.2.2 draws the line that decides how a secret is stored: at 112 bits and above, an approved hash is enough, and below that it takes a salted password hashing scheme.
- M. Jones and D. Hardt, [RFC 6750: The OAuth 2.0 Authorization Framework: Bearer Token Usage](https://www.rfc-editor.org/rfc/rfc6750.html) (IETF, 2012). The `Authorization: Bearer` header, the 401 and `WWW-Authenticate` that answer a bad token, and why a bearer token needs TLS.
- Wen Zhang, Eric Sheng, Michael Chang, Aurojit Panda, Mooly Sagiv and Scott Shenker, [Blockaid: Data Access Policy Enforcement for Web Applications](https://www.usenix.org/conference/osdi22/presentation/zhang) (OSDI, 2022). Enforcing data-access policy at the boundary between a web application and its database, on the premise that checks scattered through application code get missed.
- Shariq Rizvi, Alberto Mendelzon, S. Sudarshan and Prasan Roy, [Extending Query Rewriting Techniques for Fine-Grained Access Control](https://doi.org/10.1145/1007568.1007631) (SIGMOD, 2004). An early case for enforcing per-row access control inside the database rather than in each application, through query rewriting.
