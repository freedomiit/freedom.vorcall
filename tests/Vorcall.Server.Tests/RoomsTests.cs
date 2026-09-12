using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class RoomsTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task JoinRoom_of_a_room_already_joined_resyncs_only_the_caller()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (carol, c1) = (party.Account(1), party.Client(1));

        await c1.SendAsync(Frames.Join(Session.General));
        var state = (await c1.ExpectAsync(Kind.RoomState)).RoomState;
        Assert.Equal(Session.General, state.RoomId);
        Assert.Contains(carol.UserId, Session.MembersOf(state).Keys);
        await party.Client(0).QuietAsync();
    }

    [Fact]
    public async Task CreateRoom_slugs_the_name_and_RoomUpdated_reaches_every_connection()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var token = Names.Token();
        var name = $"Team  Chat_2 {token}";
        var id = $"team-chat-2-{token}";

        await a1.SendAsync(Frames.CreateRoom(name));
        var state = (await a1.ExpectAsync(Kind.RoomState)).RoomState;
        Assert.Equal(id, state.RoomId);
        Assert.Equal(new[] { (alice.UserId, alice.Username) }, Members(state));
        foreach (var client in party.Clients)
        {
            var room = (await client.ExpectAsync(Kind.RoomUpdated)).RoomUpdated.Room;
            Assert.Equal(id, room.RoomId);
            Assert.Equal(RoomKind.Public, room.Kind);
            Assert.Equal(name, room.Name);
            Assert.Equal(new[] { alice.UserId }, room.MemberIds.ToArray());
            Assert.Equal(alice.UserId, room.CreatedBy);
        }
    }

    [Fact]
    public async Task CreateRoom_refuses_unusable_names_and_taken_slugs()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var a1 = party.Client(0);
        var (name, _) = await party.CreateRoomAsync();

        foreach (var bad in new[] { "!!!", new string('n', 33), "   " })
        {
            await a1.SendAsync(Frames.CreateRoom(bad));
            await a1.ExpectErrorAsync(ErrorCode.InvalidRoomName, fatal: false);
        }

        foreach (var taken in new[] { name, "General" })
        {
            await a1.SendAsync(Frames.CreateRoom(taken));
            await a1.ExpectErrorAsync(ErrorCode.RoomExists, fatal: false);
        }

        await party.Client(1).QuietAsync();
    }

    [Fact]
    public async Task LeaveRoom_of_general_answers_FORBIDDEN()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        await party.Client(1).SendAsync(Frames.Leave(Session.General));
        await party.Client(1).ExpectErrorAsync(ErrorCode.Forbidden, fatal: false);
        await party.Client(0).QuietAsync();
    }

    [Fact]
    public async Task JoinRoom_is_persistent_and_announces_the_joiner_to_the_room()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var (carol, c1) = (party.Account(1), party.Client(1));
        var (_, roomId) = await party.CreateRoomAsync();

        await c1.SendAsync(Frames.Join(roomId));
        var state = (await c1.ExpectAsync(Kind.RoomState)).RoomState;
        Assert.Equal(roomId, state.RoomId);
        Assert.Equal(Ids(alice, carol), Ids(state));
        await a1.ExpectMemberJoinedAsync(roomId, carol);
        foreach (var client in new[] { c1, a1 })
        {
            var room = (await client.ExpectAsync(Kind.RoomUpdated)).RoomUpdated.Room;
            Assert.Equal(Ids(alice, carol), room.MemberIds.Order().ToArray());
        }

        await party.ReconnectAsync(Server, 1);
        var session = party.Client(1).Session;
        Assert.Contains(roomId, session.States.Keys);
        Assert.True(session.Joined(roomId, carol.UserId));
        await a1.QuietAsync();
    }

    [Fact]
    public async Task LeaveRoom_is_persistent_and_the_room_stays_visible_to_everyone()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var (carol, c1) = (party.Account(1), party.Client(1));
        var (_, roomId) = await party.CreateRoomAsync();
        await JoinAsync(party, 1, roomId);

        await c1.SendAsync(Frames.Leave(roomId));
        foreach (var client in new[] { c1, a1 })
        {
            await client.ExpectMemberLeftAsync(roomId, carol);
        }

        foreach (var client in new[] { c1, a1 })
        {
            var room = (await client.ExpectAsync(Kind.RoomUpdated)).RoomUpdated.Room;
            Assert.Equal(new[] { alice.UserId }, room.MemberIds.ToArray());
        }

        await party.ReconnectAsync(Server, 1);
        var session = party.Client(1).Session;
        Assert.DoesNotContain(roomId, session.States.Keys);
        Assert.False(session.Joined(roomId, carol.UserId));
        await a1.QuietAsync();
    }

    [Fact]
    public async Task Send_and_leave_in_a_room_not_joined_answer_NOT_A_MEMBER()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (_, roomId) = await party.CreateRoomAsync();
        var c1 = party.Client(1);

        await c1.SendAsync(Frames.Send("still here?", roomId));
        await c1.ExpectErrorAsync(ErrorCode.NotAMember, fatal: false);
        await c1.SendAsync(Frames.Leave(roomId));
        await c1.ExpectErrorAsync(ErrorCode.NotAMember, fatal: false);
        await party.Client(0).QuietAsync();
    }

    [Fact]
    public async Task Unread_and_mention_counters_are_cleared_by_MarkRead()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol", "bob");
        var alice = party.Account(0);
        var (bob, b1) = (party.Account(2), party.Client(2));
        var token = Names.Token();

        var plain = $"unread one {token}";
        var mention = $"hey <@{alice.UserId}> unread two {token}";
        long lastId = 0;
        foreach (var (text, mentioned) in new[] { (plain, Array.Empty<long>()), (mention, new[] { alice.UserId }) })
        {
            await b1.SendAsync(Frames.Send(text, Session.General));
            foreach (var client in party.Clients)
            {
                var message = (await client.ExpectAsync(Kind.Message)).Message;
                Assert.Equal(text, message.Text);
                Assert.Equal(bob.UserId, message.AuthorId);
                Assert.Equal(mentioned, message.MentionIds.ToArray());
                lastId = message.Id;
            }
        }

        await party.ReconnectAsync(Server, 0);
        var entry = party.Client(0).Session.Entry(Session.General);
        Assert.Equal((2L, 1L), (entry.Unread, entry.Mentions));
        Assert.Equal(lastId, entry.LastMessageId);

        // MarkRead has no reply, so the pong is what proves the cursor was written before the
        // reconnect below reads it.
        var a1 = party.Client(0);
        await a1.SendAsync(Frames.MarkRead(Session.General, lastId));
        await a1.PingFenceAsync(lastId);

        await party.ReconnectAsync(Server, 0);
        entry = party.Client(0).Session.Entry(Session.General);
        Assert.Equal((0L, 0L), (entry.Unread, entry.Mentions));
    }

    [Fact]
    public async Task Join_and_leave_of_an_unknown_room_answer_UNKNOWN_ROOM()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var c1 = party.Client(1);
        foreach (var frame in new[] { Frames.Join("nope"), Frames.Leave("nope"), Frames.Join("Bad!") })
        {
            await c1.SendAsync(frame);
            await c1.ExpectErrorAsync(ErrorCode.UnknownRoom, fatal: false);
        }

        await party.Client(0).QuietAsync();
    }

    // Joins one member to a room its creator (the party's first member) is alone in, and
    // consumes the frames: RoomState on the joiner, MemberJoined on the creator, RoomUpdated on
    // every socket.
    private static async Task JoinAsync(Party party, int index, string roomId)
    {
        await party.Client(index).SendAsync(Frames.Join(roomId));
        await party.Client(index).ExpectAsync(Kind.RoomState);
        await party.Client(0).ExpectMemberJoinedAsync(roomId, party.Account(index));
        foreach (var client in party.Clients)
        {
            await client.ExpectAsync(Kind.RoomUpdated);
        }
    }

    private static long[] Ids(params Account[] accounts) => accounts.Select(account => account.UserId).Order().ToArray();

    private static long[] Ids(RoomState state) => state.Members.Select(member => member.UserId).Order().ToArray();

    private static (long Id, string Name)[] Members(RoomState state)
        => state.Members.Select(member => (member.UserId, member.Username)).OrderBy(member => member.UserId).ToArray();
}
