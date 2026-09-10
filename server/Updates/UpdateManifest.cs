namespace Vorcall.Server.Updates;

// One downloadable build. Path is a bare file name inside <ReleasesDir>/<manifest version>/.
public sealed record UpdateAsset(string Path, string Sha256, long Size);

// The release the server currently offers. Keys of Platforms are the platform tags the client
// sends in Hello, e.g. "linux-x86_64".
public sealed record UpdateManifest(
    string Version,
    string Notes,
    string PublishedAt,
    string MinVersion,
    Dictionary<string, UpdateAsset> Platforms);

// The raw bytes travel next to the parsed form because manifest.sig signs those exact bytes:
// serialising the record again would produce a body the signature no longer covers.
public sealed record LoadedManifest(byte[] Bytes, string Signature, UpdateManifest Manifest);
