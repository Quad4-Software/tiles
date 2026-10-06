import Map from 'ol/Map.js';
import {defaults as defaultControls} from 'ol/control/defaults.js';
import ScaleLine from 'ol/control/ScaleLine.js';
import {fromLonLat, toLonLat} from 'ol/proj.js';
import apply from 'ol-mapbox-style';

function parseHash() {
  var m = location.hash.match(
    /^#([0-9]+(?:\.[0-9]+)?)\/(-?[0-9]+(?:\.[0-9]+)?)\/(-?[0-9]+(?:\.[0-9]+)?)/
  );
  return m ? {zoom: +m[1], lat: +m[2], lon: +m[3]} : null;
}

// Renders a MapLibre/Mapbox style.json on an OpenLayers map.
// interpolate:false keeps raster tiles sharp at fractional zooms.
export function init(styleUrl) {
  var map = new Map({
    target: 'map',
    controls: defaultControls().extend([new ScaleLine()]),
  });
  init.map = map;
  var done = apply(map, styleUrl, {interpolate: false});
  done.then(function () {
    var h = parseHash();
    if (h) {
      map.getView().setCenter(fromLonLat([h.lon, h.lat]));
      map.getView().setZoom(h.zoom);
    }
    map.on('moveend', function () {
      var c = toLonLat(map.getView().getCenter() || [0, 0]);
      var z = map.getView().getZoom();
      if (z != null) {
        history.replaceState(
          null,
          '',
          '#' + z.toFixed(2) + '/' + c[1].toFixed(5) + '/' + c[0].toFixed(5)
        );
      }
    });
  });
  return done;
}
