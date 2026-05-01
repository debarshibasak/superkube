# Superkube — common dev / build / install targets.
#
#   make help        — list targets
#   make build       — cargo build --release for the host
#   make linux       — cross-compile for x86_64 / aarch64 Linux via `cross`
#   make docker      — build the container image
#   make install     — install + start as a system service (auto-detects OS)

SHELL := /usr/bin/env bash

BIN              ?= superkube
TARGET_DIR       ?= target
DOCKER_IMAGE     ?= system32ai/superkube
DOCKER_TAG       ?= dev
LINUX_TARGET     ?= x86_64-unknown-linux-gnu
ARM_LINUX_TARGET ?= aarch64-unknown-linux-gnu

UNAME_S := $(shell uname -s)

# ---------------------------------------------------------------------------- #
# Build                                                                        #
# ---------------------------------------------------------------------------- #

.PHONY: help
help: ## Show this help.
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_.-]+:.*?## / {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

.PHONY: build
build: ## cargo build --release (host platform)
	cargo build --release

.PHONY: run
run: ## cargo run -- server
	cargo run --release -- server

.PHONY: test
test: ## cargo test
	cargo test

.PHONY: fmt
fmt: ## cargo fmt
	cargo fmt --all

.PHONY: clippy
clippy: ## cargo clippy -- -D warnings
	cargo clippy --all-targets -- -D warnings

.PHONY: clean
clean: ## remove build artifacts
	cargo clean

# ---------------------------------------------------------------------------- #
# Cross-compile (macOS host -> Linux target)                                   #
# ---------------------------------------------------------------------------- #

.PHONY: linux
linux: ## cross build for $(LINUX_TARGET) (default x86_64-unknown-linux-gnu)
	@command -v cross >/dev/null || { echo "install with: cargo install cross"; exit 1; }
	cross build --release --target $(LINUX_TARGET)
	@echo "binary at: $(TARGET_DIR)/$(LINUX_TARGET)/release/$(BIN)"

.PHONY: linux-arm
linux-arm: ## cross build for $(ARM_LINUX_TARGET)
	@command -v cross >/dev/null || { echo "install with: cargo install cross"; exit 1; }
	cross build --release --target $(ARM_LINUX_TARGET)
	@echo "binary at: $(TARGET_DIR)/$(ARM_LINUX_TARGET)/release/$(BIN)"

# ---------------------------------------------------------------------------- #
# Docker                                                                       #
# ---------------------------------------------------------------------------- #

.PHONY: docker
docker: ## build the container image
	DOCKER_BUILDKIT=1 docker build -t $(DOCKER_IMAGE):$(DOCKER_TAG) .

.PHONY: docker-run
docker-run: ## run the container (API on host :6443, SQLite volume)
	docker run --rm -it \
		-p 6443:6443 \
		-v superkube-data:/var/lib/superkube \
		--name superkube \
		$(DOCKER_IMAGE):$(DOCKER_TAG)

.PHONY: docker-push
docker-push: ## push the image (set DOCKER_IMAGE to a registry path first)
	docker push $(DOCKER_IMAGE):$(DOCKER_TAG)

# ---------------------------------------------------------------------------- #
# Publish (push to GitHub remote `origin2` + Docker Hub)                       #
# ---------------------------------------------------------------------------- #

PUBLISH_REMOTE ?= origin2
PUBLISH_BRANCH ?= $(shell git rev-parse --abbrev-ref HEAD)
# Tag at HEAD (errors if HEAD is not tagged — set PUBLISH_TAG=... to override)
PUBLISH_TAG    ?= $(shell git describe --tags --exact-match 2>/dev/null)

.PHONY: publish-git
publish-git: ## push current branch + $(PUBLISH_TAG) to $(PUBLISH_REMOTE)
	@[ -n "$(PUBLISH_TAG)" ] || { echo "no git tag at HEAD; tag the commit or pass PUBLISH_TAG=..."; exit 1; }
	git push $(PUBLISH_REMOTE) $(PUBLISH_BRANCH)
	git push $(PUBLISH_REMOTE) $(PUBLISH_TAG)

.PHONY: publish-docker
publish-docker: ## build & push $(DOCKER_IMAGE):$(PUBLISH_TAG) and :latest
	@[ -n "$(PUBLISH_TAG)" ] || { echo "no git tag at HEAD; tag the commit or pass PUBLISH_TAG=..."; exit 1; }
	DOCKER_BUILDKIT=1 docker build -t $(DOCKER_IMAGE):$(PUBLISH_TAG) -t $(DOCKER_IMAGE):latest .
	docker push $(DOCKER_IMAGE):$(PUBLISH_TAG)
	docker push $(DOCKER_IMAGE):latest

.PHONY: publish
publish: publish-git publish-docker ## publish $(PUBLISH_TAG) to GitHub ($(PUBLISH_REMOTE)) + Docker Hub

# ---------------------------------------------------------------------------- #
# System install (systemd on Linux, launchd on macOS)                          #
# ---------------------------------------------------------------------------- #

.PHONY: install
install: ## install + start as a system service (auto: linux/mac)
ifeq ($(UNAME_S),Linux)
	sudo bash deploy/install/install-linux.sh server
else ifeq ($(UNAME_S),Darwin)
	sudo bash deploy/install/install-macos.sh server
else
	@echo "unsupported OS: $(UNAME_S)"; exit 1
endif

.PHONY: install-linux
install-linux: ## install superkube-server.service on this Linux host
	sudo bash deploy/install/install-linux.sh server

.PHONY: install-linux-node
install-linux-node: ## install superkube-node.service (requires SERVER=http://...)
	@[ -n "$(SERVER)" ] || { echo "usage: make install-linux-node SERVER=http://master:6443"; exit 1; }
	sudo bash deploy/install/install-linux.sh node --server $(SERVER)

.PHONY: install-macos
install-macos: ## install dev.superkube.server launchd daemon
	sudo bash deploy/install/install-macos.sh server

.PHONY: uninstall
uninstall: ## stop + remove the system service
ifeq ($(UNAME_S),Linux)
	sudo bash deploy/install/install-linux.sh uninstall all
else ifeq ($(UNAME_S),Darwin)
	sudo bash deploy/install/install-macos.sh uninstall
else
	@echo "unsupported OS: $(UNAME_S)"; exit 1
endif
