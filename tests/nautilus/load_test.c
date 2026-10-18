/* Loads libcairn-nautilus.so the way Nautilus does (GTypeModule +
 * nautilus_module_* entry points) and checks the registered type implements
 * NautilusMenuProvider. Build/run: make -C tests/nautilus */
#define NAUTILUS_COMPILATION 1
#include <gmodule.h>
#include <libnautilus-extension/nautilus-menu-provider.h>
#include <stdio.h>

typedef struct { GTypeModule parent; } TestModule;
typedef struct { GTypeModuleClass parent; } TestModuleClass;
static gboolean tm_load (GTypeModule *m) { return TRUE; }
static void tm_unload (GTypeModule *m) {}
static void test_module_class_init (TestModuleClass *k) { G_TYPE_MODULE_CLASS (k)->load = tm_load; G_TYPE_MODULE_CLASS (k)->unload = tm_unload; }
static void test_module_init (TestModule *m) {}
G_DEFINE_TYPE (TestModule, test_module, G_TYPE_TYPE_MODULE)

int main (int argc, char **argv)
{
    GModule *lib = g_module_open (argv[1], G_MODULE_BIND_LAZY | G_MODULE_BIND_LOCAL);
    if (!lib) { fprintf (stderr, "dlopen: %s\n", g_module_error ()); return 1; }
    void (*init) (GTypeModule *); void (*list) (const GType **, int *);
    if (!g_module_symbol (lib, "nautilus_module_initialize", (gpointer *) &init) ||
        !g_module_symbol (lib, "nautilus_module_list_types", (gpointer *) &list)) { fprintf (stderr, "missing entry points\n"); return 1; }
    GTypeModule *mod = g_object_new (test_module_get_type (), NULL);
    g_type_module_use (mod);
    init (mod);
    const GType *types; int n;
    list (&types, &n);
    if (n != 1 || !g_type_is_a (types[0], NAUTILUS_TYPE_MENU_PROVIDER)) { fprintf (stderr, "type check failed\n"); return 1; }
    GObject *obj = g_object_new (types[0], NULL);
    GList *items = nautilus_menu_provider_get_file_items (NAUTILUS_MENU_PROVIDER (obj), NULL);
    printf ("ok: %s implements NautilusMenuProvider; empty selection -> %u items\n", g_type_name (types[0]), g_list_length (items));
    g_object_unref (obj);
    return 0;
}
