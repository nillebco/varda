PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
CARGO ?= cargo

.PHONY: build release install test fmt clean

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

test:
	$(CARGO) test

fmt:
	$(CARGO) fmt --check

clean:
	$(CARGO) clean
