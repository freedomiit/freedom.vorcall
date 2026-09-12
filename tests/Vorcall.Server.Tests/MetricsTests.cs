using System.Globalization;
using System.Net;
using Vorcall.Server.Api;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class MetricsTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Metrics_exposes_the_prometheus_text_format_and_counts_a_connected_client()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var before = await ScrapeAsync();
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        var during = await ScrapeAsync();

        Assert.Equal(Sample(before, "vorcall_online_users") + 1, Sample(during, "vorcall_online_users"));
        Assert.Equal(Sample(before, "vorcall_connections") + 1, Sample(during, "vorcall_connections"));
        Assert.True(Sample(during, "vorcall_ws_connections_total") > Sample(before, "vorcall_ws_connections_total"));
        Assert.Contains("# TYPE vorcall_messages_total counter\n", during);
        Assert.Contains("vorcall_rate_limited_total{kind=\"message\"} ", during);
    }

    // TestServer gives every request no peer address at all, which the endpoint treats as the
    // most local of callers, so the rule that turns a public scan away is proved on its own.
    [Theory]
    [InlineData(null, true)]
    [InlineData("127.0.0.1", true)]
    [InlineData("::1", true)]
    [InlineData("10.1.2.3", true)]
    [InlineData("172.16.0.1", true)]
    [InlineData("172.31.255.254", true)]
    [InlineData("192.168.1.1", true)]
    [InlineData("169.254.1.1", true)]
    [InlineData("fe80::1", true)]
    [InlineData("fc00::1", true)]
    [InlineData("fd12:3456::1", true)]
    [InlineData("::ffff:10.0.0.1", true)]
    [InlineData("172.32.0.1", false)]
    [InlineData("8.8.8.8", false)]
    [InlineData("::ffff:8.8.8.8", false)]
    [InlineData("2001:db8::1", false)]
    public void PrivateSource_admits_loopback_and_private_ranges_only(string? address, bool expected)
        => Assert.Equal(expected, PrivateSource.IsPrivate(address is null ? null : IPAddress.Parse(address)));

    private async Task<string> ScrapeAsync()
    {
        using var response = await Server.Client.GetAsync("/metrics");
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.StartsWith("text/plain", response.Content.Headers.ContentType?.ToString() ?? string.Empty);
        return await response.Content.ReadAsStringAsync();
    }

    private static long Sample(string exposition, string name)
    {
        var line = exposition.Split('\n').Single(candidate => candidate.StartsWith(name + " ", StringComparison.Ordinal));
        return long.Parse(line[(name.Length + 1)..], CultureInfo.InvariantCulture);
    }
}
