.PHONY: setup setup-local install install-local login token format lint test check

REPO := https://github.com/Dedaub/monitoring-cli

# The binary where cargo installs it. The targets call it by this path, so that another
# dedaub-monitoring earlier on PATH (the Python CLI lives in ~/.local/bin) is not run instead.
DM := $(or $(CARGO_INSTALL_ROOT),$(CARGO_HOME),$(HOME)/.cargo)/bin/dedaub-monitoring

# Print the installed version, and say so when a shell would run another dedaub-monitoring.
INSTALLED = $(DM) --version && { first="$$(command -v dedaub-monitoring || true)"; \
	[ "$$first" = "$(DM)" ] || echo "note: PATH resolves dedaub-monitoring to '$$first', not to $(DM)"; }

# One-shot setup: install the latest CLI, install the agent skill, and log in.
setup: install
	$(DM) install-skill
	$(DM) login

# The same, but with the CLI of this working tree (to test a branch).
setup-local: install-local
	$(DM) install-skill
	$(DM) login

# Always the latest main from GitHub, never the state of this clone.
install:
	cargo install --locked --force --git $(REPO)
	@$(INSTALLED)

# The CLI of this working tree.
install-local:
	cargo install --locked --force --path .
	@$(INSTALLED)

login:
	$(DM) login

# Print the stored refresh token for a machine without a browser. Export it as
# DEDAUB_MONITORING_REFRESH_TOKEN there. Treat the value as a password.
token:
	@$(DM) token

format:
	cargo fmt

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

# Everything CI runs, in one target.
check: lint test
