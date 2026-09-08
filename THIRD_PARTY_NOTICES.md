# Third-party notices

The vendored Rust skills in `.agents/skills/` originate from
[Dankosik/rust-cli-skills](https://github.com/Dankosik/rust-cli-skills), commit
`3ec323b33654173caff2c760b99d70c8923a6576`, copyright (c) 2026 Dankosik,
under the MIT license reproduced in [LICENSE](LICENSE).
`.agents/skills-source.json` records their exact content hashes.

The template adapts the runnable starting point, initialization, contract,
validation, and agent-workflow ideas of
[Dankosik/go-service-template-rest](https://github.com/Dankosik/go-service-template-rest),
commit `ef6a3bc9983431de065a421e6a3fced36ef886ba`, also licensed under MIT.
The Rust runtime and maintenance tools are newly written for CLI behavior.

Cargo dependencies retain their own licenses. `Cargo.lock` identifies the
resolved packages; `deny.toml` defines the project's dependency license and
advisory policy. This notice does not replace the licenses of dependencies.
