// Drive GNOME Shell's screen-sharing indicator through AT-SPI, like a user clicking
// "stop sharing". Usage (inside the host container, as flux):
//   gjs test-stop-sharing.js dump          # list named accessibles in the shell
//   gjs test-stop-sharing.js click <regex> # print the screen-space centre of the first match (then click it via flux input)
imports.gi.versions.Atspi = '2.0';
const { Atspi } = imports.gi;
Atspi.init();

function walk(node, depth, visit) {
    if (depth > 14)
        return;
    let name = '', role = '';
    try { name = node.get_name(); role = node.get_role_name(); } catch (e) { return; }
    visit(node, depth, role, name);
    let count = 0;
    try { count = node.get_child_count(); } catch (e) { /* gone */ }
    for (let i = 0; i < count; i++) {
        let child = null;
        try { child = node.get_child_at_index(i); } catch (e) { /* gone */ }
        if (child)
            walk(child, depth + 1, visit);
    }
}

const [mode, pattern] = ARGV;
const desktop = Atspi.get_desktop(0);
let done = false;
for (let i = 0; i < desktop.get_child_count() && !done; i++) {
    const app = desktop.get_child_at_index(i);
    if (app.get_name() !== 'gnome-shell')
        continue;
    walk(app, 0, (node, depth, role, name) => {
        if (done || !name)
            return;
        if (mode === 'dump') {
            print(`${'  '.repeat(depth)}${role}: ${name}`);
        } else if (mode === 'click' && new RegExp(pattern, 'i').test(name)) {
            const r = node.get_component_iface().get_extents(Atspi.CoordType.SCREEN);
            print(`center ` + (r.x + r.width / 2) + ` ` + (r.y + r.height / 2));
            done = true;
        }
    });
}
if (mode === 'click' && !done) {
    print('no matching accessible');
    imports.system.exit(1);
}
