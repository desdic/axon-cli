CARGO      ?= cargo
VENDOR_DIR := vendor
CARGO_CFG  := .cargo/config.toml
PLATFORMS  := *-unknown-linux-gnu *-unknown-linux-musl

OFFLINE    := --frozen --offline

.PHONY: all build release test clippy fmt vendor clean distclean

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

clean:
	$(CARGO) clean

distclean: clean
	rm -rf $(VENDOR_DIR) $(CARGO_CFG)
