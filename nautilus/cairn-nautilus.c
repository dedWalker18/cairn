/* Cairn integration for Nautilus 43+ (libnautilus-extension-4).
 *
 * Deliberately minimal: this code runs inside the Nautilus process, so it
 * never blocks, never does I/O and never talks to the daemon itself. It only
 * adds menu items whose actions spawn Cairn's own processes asynchronously
 * (no shell, argv only). A crash or hang in Cairn can therefore never take
 * Nautilus down with it.
 *
 *   Right-click on folder background:  "Search with Cairn…"
 *   Right-click on one folder:         Cairn ▸ Search in this folder…
 *                                            Exclude from search
 *                                            Include in search again
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#define NAUTILUS_COMPILATION 1
#include <glib.h>
#include <gio/gio.h>
#include <libnautilus-extension/nautilus-file-info.h>
#include <libnautilus-extension/nautilus-menu.h>
#include <libnautilus-extension/nautilus-menu-provider.h>

#define CAIRN_TYPE_EXTENSION (cairn_extension_get_type ())
G_DECLARE_FINAL_TYPE (CairnExtension, cairn_extension, CAIRN, EXTENSION, GObject)

struct _CairnExtension
{
    GObject parent_instance;
};

static void cairn_menu_provider_iface_init (NautilusMenuProviderInterface *iface);

G_DEFINE_DYNAMIC_TYPE_EXTENDED (CairnExtension, cairn_extension, G_TYPE_OBJECT, G_TYPE_FLAG_FINAL,
                                G_IMPLEMENT_INTERFACE_DYNAMIC (NAUTILUS_TYPE_MENU_PROVIDER,
                                                               cairn_menu_provider_iface_init))

/* Nautilus is often started without ~/.local/bin on PATH. */
static gchar *
find_program (const gchar *name)
{
    gchar *p = g_find_program_in_path (name);
    if (p != NULL)
        return p;
    p = g_build_filename (g_get_home_dir (), ".local", "bin", name, NULL);
    if (g_file_test (p, G_FILE_TEST_IS_EXECUTABLE))
        return p;
    g_free (p);
    return NULL;
}

static void
spawn_detached (const gchar *program, const gchar *arg1, const gchar *arg2, const gchar *arg3)
{
    g_autofree gchar *exe = find_program (program);
    g_autoptr (GError) error = NULL;
    const gchar *argv[5] = { NULL, NULL, NULL, NULL, NULL };

    if (exe == NULL)
    {
        g_warning ("cairn: %s not found in PATH or ~/.local/bin", program);
        return;
    }
    argv[0] = exe;
    argv[1] = arg1;
    argv[2] = arg2;
    argv[3] = arg3;
    if (!g_spawn_async (NULL, (gchar **) argv, NULL, G_SPAWN_DEFAULT, NULL, NULL, NULL, &error))
        g_warning ("cairn: failed to start %s: %s", program, error->message);
}

/* Local path of a NautilusFileInfo, or NULL for remote/virtual locations. */
static gchar *
local_path (NautilusFileInfo *info)
{
    g_autoptr (GFile) location = nautilus_file_info_get_location (info);
    return location != NULL ? g_file_get_path (location) : NULL;
}

static void
on_search (NautilusMenuItem *item, gpointer user_data)
{
    const gchar *path = g_object_get_data (G_OBJECT (item), "cairn-path");
    spawn_detached ("cairn-gui", "--scope", path, NULL);
}

static void
on_exclude (NautilusMenuItem *item, gpointer user_data)
{
    const gchar *path = g_object_get_data (G_OBJECT (item), "cairn-path");
    spawn_detached ("cairn", "exclude", "--notify", path);
}

static void
on_include (NautilusMenuItem *item, gpointer user_data)
{
    const gchar *path = g_object_get_data (G_OBJECT (item), "cairn-path");
    spawn_detached ("cairn", "include", "--notify", path);
}

static NautilusMenuItem *
make_item (const gchar *id, const gchar *label, const gchar *tip, const gchar *path, GCallback cb)
{
    NautilusMenuItem *item = nautilus_menu_item_new (id, label, tip, NULL);
    g_object_set_data_full (G_OBJECT (item), "cairn-path", g_strdup (path), g_free);
    g_signal_connect (item, "activate", cb, NULL);
    return item;
}

static GList *
get_background_items (NautilusMenuProvider *provider, NautilusFileInfo *current_folder)
{
    g_autofree gchar *path = local_path (current_folder);
    if (path == NULL)
        return NULL;
    return g_list_append (NULL, make_item ("CairnExtension::search_here", "Search with Cairn…",
                                           "Describe what you are looking for in this folder",
                                           path, G_CALLBACK (on_search)));
}

static GList *
get_file_items (NautilusMenuProvider *provider, GList *files)
{
    NautilusFileInfo *info;
    g_autofree gchar *path = NULL;
    NautilusMenuItem *top;
    NautilusMenu *submenu;

    if (files == NULL || files->next != NULL)
        return NULL; /* exactly one selected item */
    info = NAUTILUS_FILE_INFO (files->data);
    if (!nautilus_file_info_is_directory (info))
        return NULL;
    path = local_path (info);
    if (path == NULL)
        return NULL;

    top = nautilus_menu_item_new ("CairnExtension::menu", "Cairn", "Cairn search", NULL);
    submenu = nautilus_menu_new ();
    nautilus_menu_item_set_submenu (top, submenu);
    nautilus_menu_append_item (submenu, make_item ("CairnExtension::search_in", "Search in this folder…",
                                                   "Describe what you are looking for", path,
                                                   G_CALLBACK (on_search)));
    nautilus_menu_append_item (submenu, make_item ("CairnExtension::exclude", "Exclude from search",
                                                   "Remove this folder from the index and never index it",
                                                   path, G_CALLBACK (on_exclude)));
    nautilus_menu_append_item (submenu, make_item ("CairnExtension::include", "Include in search again",
                                                   "Undo a previous exclusion", path,
                                                   G_CALLBACK (on_include)));
    g_object_unref (submenu);
    return g_list_append (NULL, top);
}

static void
cairn_menu_provider_iface_init (NautilusMenuProviderInterface *iface)
{
    iface->get_file_items = get_file_items;
    iface->get_background_items = get_background_items;
}

static void cairn_extension_init (CairnExtension *self) {}
static void cairn_extension_class_init (CairnExtensionClass *klass) {}
static void cairn_extension_class_finalize (CairnExtensionClass *klass) {}

static GType types[1];

G_MODULE_EXPORT void
nautilus_module_initialize (GTypeModule *module)
{
    cairn_extension_register_type (module);
    types[0] = CAIRN_TYPE_EXTENSION;
}

G_MODULE_EXPORT void
nautilus_module_shutdown (void)
{
}

G_MODULE_EXPORT void
nautilus_module_list_types (const GType **out_types, int *num_types)
{
    *out_types = types;
    *num_types = G_N_ELEMENTS (types);
}
