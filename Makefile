.PHONY: twin-test

export PATH := $(HOME)/.cargo/bin:$(PATH)

twin-test:
	$(HOME)/.cargo/bin/cargo test -p chaosd -- --ignored twin --test-threads=1
