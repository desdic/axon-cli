CARGO      ?= cargo
VENDOR_DIR := vendor
CARGO_CFG  := .cargo/config.toml
PLATFORMS  := *-unknown-linux-gnu *-unknown-linux-musl

OFFLINE    := --frozen --offline

AUR_DIR    ?= ../aur-axon-cli
AUR_REPO   := ssh://aur@aur.archlinux.org/axon-cli.git

.PHONY: all build release test clippy fmt vendor clean distclean aur

all: build

build: $(CARGO_CFG)
	$(CARGO) build $(OFFLINE)

release: $(CARGO_CFG)
	$(CARGO) build --release $(OFFLINE)

test: $(CARGO_CFG)
	$(CARGO) test --workspace $(OFFLINE)

clippy: $(CARGO_CFG)
	$(CARGO) clippy --workspace --all-targets $(OFFLINE) -- -D warnings

fmt:
	$(CARGO) fmt --all

vendor:
	rm -rf $(CARGO_CFG) $(VENDOR_DIR)
	mkdir -p $(dir $(CARGO_CFG))
	$(CARGO) vendor-filterer --versioned-dirs \
		$(foreach p,$(PLATFORMS),--platform='$(p)') \
		$(VENDOR_DIR) > $(CARGO_CFG).tmp
	mv $(CARGO_CFG).tmp $(CARGO_CFG)

$(CARGO_CFG):
	$(MAKE) vendor

aur:
	cd pkg/arch && updpkgsums
	test -d $(AUR_DIR) || git -c init.defaultBranch=master clone $(AUR_REPO) $(AUR_DIR)
	cp pkg/arch/PKGBUILD $(AUR_DIR)/PKGBUILD
	cd $(AUR_DIR) && makepkg --printsrcinfo > .SRCINFO
	git -C $(AUR_DIR) add PKGBUILD .SRCINFO
	git -C $(AUR_DIR) status --short
	@echo "Review, then: git -C $(AUR_DIR) commit -m 'axon-cli <version>' && git -C $(AUR_DIR) push origin master"

clean:
	$(CARGO) clean

distclean: clean
	rm -rf $(VENDOR_DIR) $(CARGO_CFG)
