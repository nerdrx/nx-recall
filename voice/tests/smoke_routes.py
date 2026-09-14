"""Opt-in live routing validation. No audio capture, playback, or API calls."""
import asyncio
import json
from pathlib import Path
import tempfile

from nx_recall_voice.audio import Devices, command
from nx_recall_voice.routing import Graph, Router, snapshot


async def main():
    before = Graph(await snapshot())
    defaults = (await command("pactl", "get-default-sink"), await command("pactl", "get-default-source"))
    devices = Devices("nx_recall_voice_route_test")
    with tempfile.TemporaryDirectory() as folder:
        router = Router(str(Path.home() / ".config/vesktop"), incoming=devices.incoming, microphone=devices.microphone, journal_path=Path(folder) / "routes.json")
        selected = router.selected(before)
        assert selected['playback'] and selected['capture'], "Vesktop must be in a call for this test"
        targeted = set(selected['playback'] + selected['capture'])
        touched_ports = {p for p in before.ports if int(before.ports[p]['info']['props']['node.id']) in targeted}
        untouched = {e for e in before.links if not set(e) & touched_ports}
        def live_edges(edges, graph):
            return {e for e in edges if all(graph.valid(before.ref(p)) for p in e)}
        originals = {e for e in before.links if set(e) & touched_ports}
        try:
            await devices.start()
            await asyncio.sleep(.2)
            result = await router.reconcile()
            assert result['ready'], result
            await asyncio.sleep(1.5)
            assert (await router.reconcile())['ready']
            during = Graph(await snapshot())
            missing = live_edges(untouched, during) - during.links
            if missing:
                print("Missing stable links:", [(e, [before.ports[p]['info']['props'] for p in e]) for e in missing])
            assert not missing, "An unrelated stable link was removed"
            print(json.dumps({"live_route": result, "unrelated_links_preserved": len(untouched)}))
        finally:
            # A fresh instance proves the on-disk recovery journal is sufficient.
            recovered = Router(str(Path.home() / ".config/vesktop"), incoming=devices.incoming, microphone=devices.microphone, journal_path=Path(folder) / "routes.json")
            try:
                await recovered.restore()
            finally:
                await devices.close()
        after = Graph(await snapshot())
        assert live_edges(originals, after) <= after.links, "Original Vesktop links not restored"
        assert live_edges(untouched, after) <= after.links, "Other client links changed"
        assert defaults == (await command("pactl", "get-default-sink"), await command("pactl", "get-default-source"))
        print("Vesktop links restored from disk journal; defaults and other clients unchanged.")


if __name__ == "__main__":
    asyncio.run(main())
