PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
CARGO ?= cargo
DOCKER ?= docker
AGENTS_IMAGE ?= varda-agents:latest

.PHONY: build release install test fmt clean agents-image

build:
	$(CARGO) build

release:
	$(CARGO) build --release

install: release
	install -d "$(BINDIR)"
	install -m 0755 target/release/varda "$(BINDIR)/varda"
	install -m 0755 scripts/vclaude "$(BINDIR)/vclaude"
	install -m 0755 scripts/vcodex "$(BINDIR)/vcodex"
	install -m 0755 scripts/vcopilot "$(BINDIR)/vcopilot"
	install -m 0755 scripts/vmsbsh "$(BINDIR)/vmsbsh"
	install -m 0755 scripts/vdocksh "$(BINDIR)/vdocksh"

test:
	$(CARGO) test

fmt:
	$(CARGO) fmt --check

clean:
	$(CARGO) clean

# Build the resident/worker sandbox image (claude+codex CLIs + Rust toolchain)
# and load it into microsandbox. See Dockerfile.agents.
agents-image:
	$(DOCKER) build -f Dockerfile.agents -t $(AGENTS_IMAGE) .
	$(DOCKER) save $(AGENTS_IMAGE) | msb image load
