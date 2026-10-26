PREFIX ?= /usr/local
DESTDIR ?=

.PHONY: all check bench install install-systemd install-modprobe
all:
	cargo build --release --locked
check:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --locked
bench:
	cargo test --release --test performance --locked -- --ignored --nocapture
install:
	install -Dm755 target/release/thinkfan-next $(DESTDIR)$(PREFIX)/sbin/thinkfan-next
	install -Dm644 packaging/thinkfan-next.1 $(DESTDIR)$(PREFIX)/share/man/man1/thinkfan-next.1
	install -Dm644 README.md $(DESTDIR)$(PREFIX)/share/doc/thinkfan-next/README.md
	for file in docs/*.md examples/*.yaml; do \
		install -Dm644 "$$file" "$(DESTDIR)$(PREFIX)/share/doc/thinkfan-next/$$file"; \
	done
install-systemd:
	install -Dm644 packaging/systemd/thinkfan-next.service $(DESTDIR)/usr/lib/systemd/system/thinkfan-next.service
	sed -i 's|/usr/local/sbin/thinkfan-next|$(PREFIX)/sbin/thinkfan-next|g' $(DESTDIR)/usr/lib/systemd/system/thinkfan-next.service
	install -Dm644 packaging/systemd/thinkfan-next-sleep.service $(DESTDIR)/usr/lib/systemd/system/thinkfan-next-sleep.service

install-modprobe:
	install -Dm644 packaging/modprobe.d/thinkfan-next.conf $(DESTDIR)/usr/lib/modprobe.d/thinkfan-next.conf
