# Builds the Nautilus extension against the system libnautilus-extension.so.4.
# Uses pkg-config headers when libnautilus-extension-dev is installed,
# otherwise the vendored 46.4 headers (API is stable across 43..46).
CC      ?= cc
CFLAGS  ?= -O2 -g
WARN    := -Wall -Wextra -Wno-unused-parameter -Werror=implicit-function-declaration -Werror=incompatible-pointer-types
HARDEN  := -fstack-protector-strong -D_FORTIFY_SOURCE=2 -fPIC
PKGS    := glib-2.0 gobject-2.0 gio-2.0 gmodule-2.0
HAVE_DEV := $(shell pkg-config --exists libnautilus-extension-4 && echo yes)
ifeq ($(HAVE_DEV),yes)
  NAUT_CFLAGS := $(shell pkg-config --cflags libnautilus-extension-4)
  NAUT_LIBS   := $(shell pkg-config --libs libnautilus-extension-4)
  EXTDIR      := $(shell pkg-config --variable=extensiondir libnautilus-extension-4)
else
  NAUT_CFLAGS := -Ivendor
  NAUT_LIBS   := -l:libnautilus-extension.so.4
  EXTDIR      := $(firstword $(wildcard /usr/lib/x86_64-linux-gnu/nautilus/extensions-4 /usr/lib64/nautilus/extensions-4 /usr/lib/nautilus/extensions-4))
endif

libcairn-nautilus.so: cairn-nautilus.c
	$(CC) $(CFLAGS) $(WARN) $(HARDEN) -shared -Wl,-z,relro,-z,now,--as-needed -o $@ $< \
	  $(NAUT_CFLAGS) $(shell pkg-config --cflags $(PKGS)) $(NAUT_LIBS) $(shell pkg-config --libs $(PKGS))

# Nautilus only loads extensions from its system directory: this step needs root.
install: libcairn-nautilus.so
	@test -n "$(EXTDIR)" || (echo "Nautilus extension dir not found" && false)
	install -m 0644 libcairn-nautilus.so $(EXTDIR)/libcairn-nautilus.so
	@echo "installed to $(EXTDIR); restart Nautilus: nautilus -q"

uninstall:
	rm -f $(EXTDIR)/libcairn-nautilus.so

clean:
	rm -f libcairn-nautilus.so
.PHONY: install uninstall clean
