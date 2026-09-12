using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class PresenceTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Hello_answers_Welcome_and_RoomState_and_the_others_hear_MemberJoined()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var carol = await Accounts.RegisterAsync(Server, "carol");

        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        Assert.Contains(alice.UserId, Session.MembersOf(a1.Session.GeneralState).Keys);

        await using var c1 = await WsClient.ConnectAsync(Server, carol);
        await a1.ExpectMemberJoinedAsync(Session.General, carol);

        var members = Session.MembersOf(c1.Session.GeneralState);
        Assert.Equal(alice.Username, members[alice.UserId]);
        Assert.Equal(carol.Username, members[carol.UserId]);
    }

    [Fact]
    public async Task RoomList_at_welcome_lists_general_joined_with_clean_counters()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);

        var entry = a1.Session.Entry(Session.General);
        Assert.Equal(RoomKind.Public, entry.Room.Kind);
        Assert.NotEmpty(entry.Room.Name);
        Assert.True(a1.Session.Joined(Session.General, alice.UserId));

        // A fresh account's read cursor starts at the newest message, so it owes nothing.
        Assert.Equal(0L, entry.Unread);
        Assert.Equal(0L, entry.Mentions);
    }
}
