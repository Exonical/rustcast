// Fullscreen GTK4 window logging pointer/key/scroll events to /tmp/flux-probe.log.
// Run in the container's GNOME session: gjs test-input-probe.js
imports.gi.versions.Gtk = '4.0';
const { Gtk, Gio, GLib } = imports.gi;

const log = [];
function note(s) {
    log.push(s);
    GLib.file_set_contents('/tmp/flux-probe.log', log.join('\n') + '\n');
}

const app = new Gtk.Application({ application_id: 'dev.flux.Probe', flags: Gio.ApplicationFlags.NON_UNIQUE });
app.connect('activate', () => {
    const win = new Gtk.ApplicationWindow({ application: app, title: 'flux-probe' });
    win.fullscreen();
    const key = new Gtk.EventControllerKey();
    key.connect('key-pressed', (_c, keyval, keycode) => { note(`key-pressed keyval=${keyval} keycode=${keycode}`); return false; });
    win.add_controller(key);
    const motion = new Gtk.EventControllerMotion();
    motion.connect('motion', (_c, x, y) => note(`motion ${Math.round(x)},${Math.round(y)}`));
    win.add_controller(motion);
    const scroll = new Gtk.EventControllerScroll({ flags: Gtk.EventControllerScrollFlags.BOTH_AXES });
    scroll.connect('scroll', (_c, dx, dy) => { note(`scroll dx=${dx} dy=${dy}`); return false; });
    win.add_controller(scroll);
    const click = new Gtk.GestureClick({ button: 0 });
    click.connect('pressed', (g, _n, x, y) => note(`button-pressed ${g.get_current_button()} at ${Math.round(x)},${Math.round(y)}`));
    win.add_controller(click);
    win.present();
    note('ready');
});
app.run([]);
