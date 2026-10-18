PKGS := glib-2.0 gobject-2.0 gmodule-2.0 gio-2.0
run: load_test ../../nautilus/libcairn-nautilus.so
	./load_test $(abspath ../../nautilus/libcairn-nautilus.so)
load_test: load_test.c
	$(CC) -O1 -g -o $@ $< -I../../nautilus/vendor $(shell pkg-config --cflags --libs $(PKGS)) -l:libnautilus-extension.so.4
../../nautilus/libcairn-nautilus.so:
	$(MAKE) -C ../../nautilus
