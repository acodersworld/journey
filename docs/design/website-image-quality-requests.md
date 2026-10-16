# Website image quality requests

The website requests a storage-generated JPEG at the configured quality for
displayed images. The default quality is 80. The request uses
`Object-Image-Quality` and `Object-Image-Cache: true` without a representation,
range, dimension, format, fit, or byte-limit header. The same media proxy serves
authenticated and shared views, so browser media URLs stay the same. Video
playback, video posters, and WhatsApp reduced images retain their separate
request paths. Posters and WhatsApp requests also enable storage derivative
caching.

The proxy accepts a successful quality response only with status 200, JPEG
content type, and positive content length. It sends those generated response
headers to the browser and omits original-object range headers. A 413 or 415
conversion response causes an original-image request; other failures follow
normal error handling. Original downloads always use the original request and
retain its content type and attachment filename.

The optional website disk cache identifies quality JPEGs by storage key,
representation, and quality. Original downloads have a distinct identity.
Authorization runs before each cache lookup. A conversion fallback does not
populate a quality entry. No database format or browser URL changed.

Deploy the storage implementation of quality and derivative-cache headers
before enabling the updated website binary.
