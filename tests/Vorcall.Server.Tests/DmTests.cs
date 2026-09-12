using System.Net;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class DmTests(ServerFixture fixture)
{
    // Far past any serial id this suite's database will reach.
    private const long UnknownId = 9_000_000;

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task OpenDm_opens_a_channel_only_its_two_members_ever_see()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, bob) = (party.Account(0), party.Account(1));
        var dmId = await party.OpenDmAsync(0, 1);
        await party.Client(2).QuietAsync();

        // The id is the server's, so it is not something a third party could guess its way to: it is
        // simply not in their world at all, here or after a fresh hello.
        await party.ReconnectAsync(Server, 2);
        Assert.DoesNotContain(dmId, party.Client(2).Session.Channels.Keys);

        await party.ReconnectAsync(Server, 0);
        var session = party.Client(0).Session;
        var channel = session.Channel(dmId);
        Assert.Equal(ChannelKind.Dm, channel.Kind);
        Assert.Equal(string.Empty, channel.Name);
        Assert.Equal(new[] { alice.UserId, bob.UserId }.Order().ToArray(), channel.DmMemberIds.Order().ToArray());
        Assert.Contains(dmId, session.ReadStates.Keys);
    }

    [Fact]
    public async Task OpenDm_again_resyncs_the_caller_alone_and_oneself_or_a_stranger_answers_UNKNOWN_USER()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var dmId = await party.OpenDmAsync(0, 1);

        // An idempotent resync: the other party already knows about the channel, so only the caller is
        // answered, and with the same pair a first open sends.
        await b1.SendAsync(Frames.OpenDm(alice.UserId));
        var frames = await b1.ExpectSequenceAsync(Kind.ChannelUpserted, Kind.VoiceState);
        Assert.Equal(dmId, frames[0].ChannelUpserted.Channel.Id);
        Assert.Equal(dmId, frames[1].VoiceState.ChannelId);
        await b1.QuietAsync();
        await a1.QuietAsync();

        // A DM with yourself, and one with an account that does not exist, are the same answer.
        foreach (var userId in new[] { alice.UserId, UnknownId })
        {
            await a1.SendAsync(Frames.OpenDm(userId));
            await a1.ExpectErrorAsync(ErrorCode.UnknownUser, fatal: false);
        }
    }

    [Fact]
    public async Task DM_messages_and_history_stay_between_the_two_members()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (a1, b1, c1) = (party.Client(0), party.Client(1), party.Client(2));
        var (bob, carol) = (party.Account(1), party.Account(2));
        var dmId = await party.OpenDmAsync(0, 1);

        var secret = $"dm only {Names.Token()}";
        await a1.SendAsync(Frames.Send(secret, dmId));
        foreach (var client in new[] { a1, b1 })
        {
            var message = (await client.ExpectAsync(Kind.Message)).Message;
            Assert.Equal(dmId, message.ChannelId);
            Assert.Equal(secret, message.Text);
        }

        await c1.QuietAsync();

        // A DM is visible to its two members by membership, so an outsider is missing VIEW_CHANNEL in
        // it like in any other channel it may not see.
        var outsider = await History.RawAsync(Server, carol.Access, History.Id(dmId));
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);
        Assert.Equal(PermNames.Name(Perm.ViewChannel), outsider.Detail);

        var page = await History.PageAsync(Server, bob.Access, dmId);
        Assert.Equal(secret, page.Messages[^1].Text);
    }

    [Fact]
    public async Task A_disconnect_announces_the_member_offline_once_however_many_channels_it_shared()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (a1, b1, c1) = (party.Client(0), party.Client(1), party.Client(2));
        var bob = party.Account(1);
        await party.OpenDmAsync(0, 1);

        // Presence is a property of the member, not of a channel: alice shares both general and a DM
        // with bob and still hears about the disconnect exactly once, as does carol, who shares only
        // general.
        await b1.CloseAsync();
        foreach (var client in new[] { a1, c1 })
        {
            await client.ExpectMemberUpdatedAsync(bob, online: false);
            await client.QuietAsync();
        }
    }
}
