using System.Net;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class UpdatesTests(ServerFixture fixture)
{
    private const string ManifestPath = "/api/updates/manifest";
    private const string AssetPath = $"/api/updates/{Releases.Version}/{Releases.AssetName}";

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Manifest_is_served_verbatim_with_its_signature_in_a_header()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var response = await Proto.GetAsync(Server, ManifestPath, alice.Access);
        Assert.Equal(HttpStatusCode.OK, response.Status);
        Assert.Equal(Releases.ManifestBytes, response.Body);
        Assert.Equal(Releases.Signature, response.Header("X-Vorcall-Manifest-Signature"));
        Assert.Contains("application/json", response.Header("Content-Type") ?? string.Empty);
        Assert.Equal("no-store", response.Header("Cache-Control"));
    }

    [Fact]
    public async Task The_published_asset_downloads_with_the_manifests_hash_as_its_ETag()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var response = await Proto.GetAsync(Server, AssetPath, alice.Access);
        Assert.Equal(HttpStatusCode.OK, response.Status);
        Assert.Equal(Releases.AssetBytes, response.Body);
        Assert.Equal("application/octet-stream", response.Header("Content-Type"));
        Assert.Equal($"\"{Releases.AssetSha256}\"", response.Header("ETag"));
    }

    [Fact]
    public async Task Unpublished_versions_and_files_answer_404()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        foreach (var path in new[]
        {
            $"/api/updates/1.0.0/{Releases.AssetName}",
            $"/api/updates/{Releases.Version}/manifest.json",
            $"/api/updates/{Releases.Version}/vorcall-windows-x86_64.exe",
        })
        {
            Assert.Equal(HttpStatusCode.NotFound, (await Proto.GetAsync(Server, path, alice.Access)).Status);
        }
    }

    [Fact]
    public async Task Update_endpoints_require_a_bearer()
    {
        foreach (var path in new[] { ManifestPath, AssetPath })
        {
            var response = await Proto.GetAsync(Server, path);
            Assert.Equal(HttpStatusCode.Unauthorized, response.Status);
            Assert.StartsWith("Bearer", response.Header("WWW-Authenticate") ?? string.Empty);
        }
    }

    [Fact]
    public async Task A_missing_signature_means_there_is_no_release_to_offer()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var signature = Path.Combine(Server.ReleasesDir, "manifest.sig");
        var parked = signature + ".away";
        File.Move(signature, parked);
        try
        {
            Assert.Equal(HttpStatusCode.NotFound, (await Proto.GetAsync(Server, ManifestPath, alice.Access)).Status);
            Assert.Equal(HttpStatusCode.NotFound, (await Proto.GetAsync(Server, AssetPath, alice.Access)).Status);
        }
        finally
        {
            File.Move(parked, signature);
        }

        Assert.Equal(HttpStatusCode.OK, (await Proto.GetAsync(Server, ManifestPath, alice.Access)).Status);
    }
}
