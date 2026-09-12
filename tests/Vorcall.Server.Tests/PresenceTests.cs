using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

// PROTOCOL.md § Server session state machine and the Hello sequence: presence is a flag on a member,
// not a membership. Every account belongs to the one server, so there is nothing to join and nothing
// to leave — a socket opening and closing is all that ever changes.
[Collection(ServerCollection.Name)]
public sealed class PresenceTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task The_snapshot_carries_every_member_and_a_session_opening_or_closing_is_a_MemberUpdated()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        Assert.True(a1.Session.Member(alice.UserId).Online);

        // A registration reaches every online member at once, offline: the mirror can resolve the
        // new account from then on, and it has no session yet.
        var carol = await Accounts.RegisterAsync(Server, "carol");
        await a1.ExpectMemberUpdatedAsync(carol, online: false);

        var c1 = await WsClient.ConnectAsync(Server, carol);
        try
        {
            await a1.ExpectMemberUpdatedAsync(carol, online: true);

            // Carol's own snapshot names both members, and says which of them hold a session.
            Assert.Equal(alice.Username, c1.Session.Member(alice.UserId).Username);
            Assert.True(c1.Session.Member(alice.UserId).Online);
            Assert.True(c1.Session.Member(carol.UserId).Online);
        }
        finally
        {
            await c1.DisposeAsync();
        }

        // The member stays on the server with the session gone; only the flag changes.
        await a1.ExpectMemberUpdatedAsync(carol, online: false);
    }

    [Fact]
    public async Task The_snapshot_lists_general_with_clean_counters()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);

        var general = a1.Session.General;
        Assert.Equal(ChannelKind.Text, general.Kind);
        Assert.NotEmpty(general.Name);

        // A fresh account's read cursor starts at the newest message, so it owes nothing.
        var read = a1.Session.Read(a1.Session.GeneralId);
        Assert.Equal(0L, read.Unread);
        Assert.Equal(0L, read.Mentions);
    }
}
