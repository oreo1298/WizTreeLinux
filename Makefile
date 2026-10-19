# Dendro build and install rules.
#
#   make                            build an optimized binary (run as your normal user)
#   sudo make install               install to /usr/local
#   make install PREFIX=~/.local    install for your user only (no sudo needed)
#   sudo make uninstall             remove again (use the same PREFIX as for install)
#
# Packagers: make install DESTDIR="$pkgdir" PREFIX=/usr

APP_ID      := io.github.oreo1298.Dendro
PREFIX      ?= /usr/local
DESTDIR     ?=
BINDIR      ?= $(PREFIX)/bin
DATADIR     ?= $(PREFIX)/share
CARGO       ?= cargo
CARGO_FLAGS ?= --locked
BIN_SRC     ?= target/release/dendro

# polkit only reads policies from /usr/share/polkit-1/actions, so the policy
# is installed for system-wide prefixes only. Without it, "Scan as
# Administrator" still works but shows a generic password prompt.
ifneq ($(filter /usr /usr/local,$(PREFIX)),)
POLKIT_DIR  ?= /usr/share/polkit-1/actions
endif

.PHONY: all build check install uninstall clean

all: build

build:
	$(CARGO) build --release $(CARGO_FLAGS)

check:
	$(CARGO) test --release $(CARGO_FLAGS)

install:
	@test -x "$(BIN_SRC)" || { echo "error: $(BIN_SRC) not found. Run 'make' first (without sudo)."; exit 1; }
	install -Dm755 "$(BIN_SRC)" "$(DESTDIR)$(BINDIR)/dendro"
	install -d "$(DESTDIR)$(DATADIR)/applications"
	sed 's|@BINDIR@|$(BINDIR)|g' data/$(APP_ID).desktop.in > "$(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop"
	chmod 644 "$(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop"
	install -Dm644 data/$(APP_ID).metainfo.xml "$(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml"
	install -Dm644 data/icons/$(APP_ID).svg "$(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg"
	install -Dm644 data/icons/$(APP_ID)-symbolic.svg "$(DESTDIR)$(DATADIR)/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg"
ifneq ($(POLKIT_DIR),)
	install -d "$(DESTDIR)$(POLKIT_DIR)"
	sed 's|@BINDIR@|$(BINDIR)|g' data/$(APP_ID).policy.in > "$(DESTDIR)$(POLKIT_DIR)/$(APP_ID).policy"
	chmod 644 "$(DESTDIR)$(POLKIT_DIR)/$(APP_ID).policy"
endif
	@if [ -z "$(DESTDIR)" ]; then \
		gtk-update-icon-cache -qtf "$(DATADIR)/icons/hicolor" 2>/dev/null || true; \
		update-desktop-database -q "$(DATADIR)/applications" 2>/dev/null || true; \
	fi
	@echo "Installed Dendro to $(DESTDIR)$(PREFIX)"

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/dendro"
	rm -f "$(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop"
	rm -f "$(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml"
	rm -f "$(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg"
	rm -f "$(DESTDIR)$(DATADIR)/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg"
ifneq ($(POLKIT_DIR),)
	rm -f "$(DESTDIR)$(POLKIT_DIR)/$(APP_ID).policy"
endif
	@if [ -z "$(DESTDIR)" ]; then \
		gtk-update-icon-cache -qtf "$(DATADIR)/icons/hicolor" 2>/dev/null || true; \
		update-desktop-database -q "$(DATADIR)/applications" 2>/dev/null || true; \
	fi
	@echo "Removed Dendro from $(DESTDIR)$(PREFIX)"

clean:
	$(CARGO) clean
