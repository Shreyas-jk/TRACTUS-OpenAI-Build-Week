.PHONY: twin-clean twin-test

export PATH := $(HOME)/.cargo/bin:$(PATH)

twin-clean:
	@docker ps --format '{{.Names}}' | while IFS= read -r name; do \
		case "$$name" in chaostwin-pool-*|chaostwin-twin-*) docker kill "$$name" >/dev/null ;; esac; \
	done

twin-test: twin-clean
	$(HOME)/.cargo/bin/cargo test -p chaosd -- --ignored twin --test-threads=1
