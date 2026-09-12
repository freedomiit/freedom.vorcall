using System.Globalization;
using System.Net;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class HistoryTests(ServerFixture fixture)
{
    // Far past any serial id this suite's database will reach.
    private const long UnknownId = 9_000_000;

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task History_pages_100_ascending_with_an_exclusive_before_and_clamped_limits()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var token = Names.Token();
        long general;
        await using (var a1 = await WsClient.ConnectAsync(Server, alice))
        {
            general = a1.Session.GeneralId;

            // Unpaced on purpose: ServerFixture raises Vorcall:MessageBurst on this host so that only
            // a dedicated factory's own low limit is ever the one a test trips.
            for (var index = 0; index < 105; index++)
            {
                await a1.SendAsync(Frames.Send($"seed {token} {index}", general));
                await a1.ExpectAsync(Kind.Message, TimeSpan.FromSeconds(10));
            }
        }

        var page = await History.PageAsync(Server, alice.Access, general);
        var ids = page.Messages.Select(message => message.Id).ToArray();
        Assert.Equal(100, ids.Length);
        Assert.Equal(ids.Order().ToArray(), ids);
        Assert.True(page.HasMore);
        Assert.All(page.Messages, message => Assert.Equal(general, message.ChannelId));
        Assert.Contains(page.Messages, message => message.AuthorId > 0);

        var older = await History.PageAsync(
            Server,
            alice.Access,
            general,
            before: ids[0].ToString(CultureInfo.InvariantCulture));
        Assert.NotEmpty(older.Messages);
        Assert.All(older.Messages, message => Assert.True(message.Id < ids[0]));

        Assert.Single((await History.PageAsync(Server, alice.Access, general, limit: "0")).Messages);
        Assert.Equal(100, (await History.PageAsync(Server, alice.Access, general, limit: "1000")).Messages.Count);
    }

    [Fact]
    public async Task History_refuses_unusable_query_values()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        var general = History.Id(a1.Session.GeneralId);

        // The channel is required and is digits only, so a missing, non-numeric or non-positive value
        // names no channel at all. It is also checked before limit and before, which is why those two
        // cases below carry a usable channel.
        foreach (var channel in new string?[] { null, "bad!", "0", "-1", " 1", "12345678901234567890" })
        {
            var refused = await History.RawAsync(Server, alice.Access, channel: channel);
            Assert.Equal(HttpStatusCode.BadRequest, refused.Status);
            Assert.Equal("channel", refused.Detail);
        }

        // A voice channel exists and is visible, but holds no messages to page through.
        var voice = await History.RawAsync(Server, alice.Access, channel: History.Id(a1.Session.GeneralVoiceId));
        Assert.Equal(HttpStatusCode.BadRequest, voice.Status);
        Assert.Equal("channel", voice.Detail);

        Assert.Equal(HttpStatusCode.BadRequest, (await History.RawAsync(Server, alice.Access, general, before: "abc")).Status);
        Assert.Equal(HttpStatusCode.BadRequest, (await History.RawAsync(Server, alice.Access, general, before: "0")).Status);
        Assert.Equal(HttpStatusCode.BadRequest, (await History.RawAsync(Server, alice.Access, general, limit: "abc")).Status);
    }

    [Fact]
    public async Task History_of_a_channel_the_caller_cannot_view_answers_403_naming_VIEW_CHANNEL()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "carol");
        var (owner, carol) = (party.Account(0), party.Account(1));
        var (_, id) = await party.CreateChannelAsync();

        // Sight of a channel is a permission, so it is an override that takes it away; the frames this
        // causes are spelled out because they differ per member.
        await party.Client(0).SendAsync(
            Frames.SetOverride(id, Frames.MemberOverride(carol.UserId, deny: (ulong)Perm.ViewChannel)));
        await party.Client(1).ExpectChannelDeletedAsync(id);
        await party.Client(0).ExpectChannelUpsertedAsync(id);

        var outsider = await History.RawAsync(Server, carol.Access, History.Id(id));
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);
        Assert.Equal(PermNames.Name(Perm.ViewChannel), outsider.Detail);

        // A channel that never existed answers exactly the same, so a page request cannot be used to
        // discover which ids are real.
        var ghost = await History.RawAsync(Server, carol.Access, History.Id(UnknownId));
        Assert.Equal(HttpStatusCode.Forbidden, ghost.Status);
        Assert.Equal(PermNames.Name(Perm.ViewChannel), ghost.Detail);

        await History.PageAsync(Server, owner.Access, id);
    }
}
