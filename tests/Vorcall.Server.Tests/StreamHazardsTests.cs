using System.Diagnostics;
using System.Net;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Hosting;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

// The streamed-file proxy holds a reader's request open across a round trip to somebody else's
// client, so the interesting cases are all about that wait ending badly: the owner's socket
// changing under it, filling up, or going away, the host stopping, and the reader leaving first.
// Every one of them has to end the wait rather than leave a request hanging on a promise nobody
// is going to keep.
[Collection(ServerCollection.Name)]
public sealed class StreamHazardsTests(ServerFixture fixture)
{
    // The shared host's sender timeout is thirty seconds, so anything answered inside this bound
    // was answered by the thing that happened and not by the clock.
    private static readonly TimeSpan Promptly = TimeSpan.FromSeconds(5);

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task A_transfer_survives_the_owner_replacing_the_socket_the_request_went_to()
    {
        await using var party = await Party.ConnectAsync(Server, "vanja");
        var vanja = party.Account(0);
        var content = Blob.Of(64 * 1024);
        var file = await OfferAsync(vanja, party.GeneralId, content.Length);
        var sender = new Sender(Server, vanja, content);

        var fetch = StreamApi.FetchAsync(Server, vanja.Access, file.Id);
        var request = await Sender.NextRequestAsync(party.Client(0));

        // The request reached a socket that is about to be replaced. A replacement is not the
        // account going offline, so the reader keeps waiting and the new session answers.
        await party.ReconnectAsync(Server, 0);
        await party.Client(0).QuietAsync();

        var push = await sender.PushAsync(request);
        Assert.Equal(HttpStatusCode.NoContent, push.Status);

        var served = await fetch;
        Assert.Equal(HttpStatusCode.OK, served.Status);
        Assert.Equal(content, served.Body);
    }

    [Fact]
    public async Task An_owner_whose_outbox_is_full_cannot_be_asked_and_is_closed_as_a_slow_consumer()
    {
        // The tight host, so a miss costs a second rather than thirty: an outbox is held full by
        // writers racing the send loop, and they do not win every time.
        var server = await fixture.TightStreamsAsync();
        await using var party = await Party.ConnectAsync(server, "wilma");
        var general = party.GeneralId;

        var stopped = await Accounts.RegisterAsync(server, "xanthe");
        var offered = await StreamApi.OfferAsync(
            server,
            stopped.Access,
            general,
            new StreamOffer { FileName = "unread.bin", Size = 4096 });
        Assert.Equal(HttpStatusCode.Created, offered.Status);
        var file = offered.As(StreamedFile.Parser);

        await using var deaf = await DeafSocket.ConnectAsync(server, stopped);

        ProtoResponse? refused = null;
        for (var attempt = 0; attempt < 8 && refused is null; attempt++)
        {
            ProtoResponse answer;
            using (deaf.HoldOutboxFull())
            {
                answer = await StreamApi.FetchAsync(server, stopped.Access, file.Id);
            }

            if (answer.Status == HttpStatusCode.Conflict)
            {
                refused = answer;
                break;
            }

            // The request found room in the outbox after all: nobody is reading it, so the wait
            // runs out and the next attempt tries again.
            Assert.True(
                answer.Status == HttpStatusCode.GatewayTimeout,
                $"attempt {attempt}: {(int)answer.Status} {answer.Detail}");
        }

        Assert.True(refused is not null, "the outbox was never full when the request was sent");

        // A frame the outbox refused is a member that has stopped reading, which for this reader
        // is an owner as good as offline and for that member is the end of its session.
        Assert.Equal("the sender is offline", refused!.Detail);
        Assert.Equal(1013, await deaf.DrainToCloseAsync(TimeSpan.FromSeconds(30)));
    }

    [Fact]
    public async Task An_owner_that_goes_offline_ends_a_waiting_fetch_at_once_rather_than_at_the_timeout()
    {
        await using var party = await Party.ConnectAsync(Server, "yvonne");
        var (yvonne, y1) = (party.Account(0), party.Client(0));
        var file = await OfferAsync(yvonne, party.GeneralId, 4096);

        var fetch = StreamApi.FetchAsync(Server, yvonne.Access, file.Id);
        await Sender.NextRequestAsync(y1);

        var watch = Stopwatch.StartNew();
        await y1.CloseAsync();
        var answer = await WithinAsync(fetch, "the fetch");
        Assert.Equal(HttpStatusCode.Conflict, answer.Status);
        Assert.Equal("the sender is offline", answer.Detail);
        Assert.True(watch.Elapsed < Promptly, $"the fetch waited {watch.Elapsed.TotalSeconds:0.#}s");
    }

    [Fact]
    public async Task An_owner_that_is_kicked_or_banned_ends_a_waiting_fetch_at_once_too()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "zelda", "alba");
        var (o1, zelda, alba) = (party.Client(0), party.Account(1), party.Account(2));
        var general = party.GeneralId;

        var kicked = await OfferAsync(zelda, general, 4096);
        var kickFetch = StreamApi.FetchAsync(Server, zelda.Access, kicked.Id);
        await Sender.NextRequestAsync(party.Client(1));

        var kickWatch = Stopwatch.StartNew();
        await o1.SendAsync(Frames.KickMember(zelda.UserId));
        await party.Client(1).ExpectErrorAsync(ErrorCode.Kicked, fatal: true);
        var afterKick = await WithinAsync(kickFetch, "the kicked owner's fetch");
        Assert.Equal(HttpStatusCode.Conflict, afterKick.Status);
        Assert.True(kickWatch.Elapsed < Promptly, $"the fetch waited {kickWatch.Elapsed.TotalSeconds:0.#}s");
        await o1.ExpectMemberUpdatedAsync(zelda, online: false);
        await party.Client(2).ExpectMemberUpdatedAsync(zelda, online: false);

        var banned = await OfferAsync(alba, general, 4096);
        var banFetch = StreamApi.FetchAsync(Server, alba.Access, banned.Id);
        await Sender.NextRequestAsync(party.Client(2));

        var banWatch = Stopwatch.StartNew();
        await o1.SendAsync(Frames.BanMember(alba.UserId, "no"));
        await party.Client(2).ExpectErrorAsync(ErrorCode.Banned, fatal: true);
        var afterBan = await WithinAsync(banFetch, "the banned owner's fetch");
        Assert.Equal(HttpStatusCode.Conflict, afterBan.Status);
        Assert.True(banWatch.Elapsed < Promptly, $"the fetch waited {banWatch.Elapsed.TotalSeconds:0.#}s");

        // A ban ends the session and then removes the member, in that order.
        var frames = await o1.ExpectSequenceAsync(Kind.MemberUpdated, Kind.MemberRemoved);
        Assert.Equal(alba.UserId, frames[1].MemberRemoved.UserId);
    }

    [Fact]
    public async Task A_claimed_transfer_finishes_even_though_the_owners_socket_drops_under_it()
    {
        await using var party = await Party.ConnectAsync(Server, "berit");
        var (berit, b1) = (party.Account(0), party.Client(0));

        // Large enough that the reader's response cannot be buffered whole: the copy is still
        // running when the socket goes, which is the only moment at which this could go wrong.
        var content = Blob.Of(16 << 20);
        var file = await OfferAsync(berit, party.GeneralId, content.Length);
        var sender = new Sender(Server, berit, content);

        var open = StreamApi.OpenAsync(Server, berit.Access, file.Id);
        var request = await Sender.NextRequestAsync(b1);
        var push = sender.PushAsync(request);

        using var response = await open;
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var body = await response.Content.ReadAsStreamAsync(CancellationToken.None);
        var head = new byte[8192];
        await body.ReadExactlyAsync(head);
        Assert.Equal(content[..head.Length], head);
        Assert.False(push.IsCompleted, "the copy finished before the reader had read anything");

        // A push already under way rides its own HTTP request and needs nothing from the socket.
        await b1.CloseAsync();

        using var rest = new MemoryStream();
        await body.CopyToAsync(rest);
        Assert.Equal(content.Length - head.Length, rest.Length);
        Assert.Equal(content[head.Length..], rest.ToArray());
        Assert.Equal(HttpStatusCode.NoContent, (await push).Status);
    }

    [Fact]
    public async Task A_reader_that_leaves_mid_copy_stops_the_push_rather_than_being_pumped_at()
    {
        await using var party = await Party.ConnectAsync(Server, "carola");
        var (carola, c1) = (party.Account(0), party.Client(0));
        var content = Blob.Of(16 << 20);
        var file = await OfferAsync(carola, party.GeneralId, content.Length);
        var sender = new Sender(Server, carola, content);

        var open = StreamApi.OpenAsync(Server, carola.Access, file.Id);
        var request = await Sender.NextRequestAsync(c1);
        var push = sender.PushAsync(request);

        var response = await open;
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var body = await response.Content.ReadAsStreamAsync(CancellationToken.None);
        var head = new byte[8192];
        await body.ReadExactlyAsync(head);

        // Every write to the reader is awaited before the owner's body is read again, so a reader
        // that stops reading stops the copy: the push is still in flight a moment later, which is
        // what says the server is not pulling gigabytes into a socket nobody is draining.
        await Task.Delay(500);
        Assert.False(push.IsCompleted, "the copy ran on past a reader that had stopped reading");

        response.Dispose();
        var pushed = await WithinAsync(push, "the push");
        Assert.Equal(HttpStatusCode.Gone, pushed.Status);
        Assert.Equal("the reader went away", pushed.Detail);
    }

    [Fact]
    public async Task Two_readers_of_one_streamed_file_are_served_at_the_same_time()
    {
        await using var party = await Party.ConnectAsync(Server, "dorit", "eliza");
        var (dorit, d1, eliza) = (party.Account(0), party.Client(0), party.Account(1));
        var general = party.GeneralId;
        var content = Blob.Of(200_000);
        var file = await OfferAsync(dorit, general, content.Length);
        var sender = new Sender(Server, dorit, content);

        var text = $"for both of you {Names.Token()}";
        await d1.SendAsync(Frames.SendStreamed(text, general, file.Id));
        await party.ExpectMessageEverywhereAsync(text, general);

        var mine = StreamApi.FetchAsync(Server, dorit.Access, file.Id);
        var theirs = StreamApi.FetchAsync(Server, eliza.Access, file.Id, "bytes=1000-1999");

        // Two ranges asked of one owner, each its own transfer; answered in whichever order they
        // arrived, since a transfer is named by its id and not by its turn.
        var first = await Sender.NextRequestAsync(d1);
        var second = await Sender.NextRequestAsync(d1);
        Assert.NotEqual(first.TransferId, second.TransferId);
        Assert.Equal(HttpStatusCode.NoContent, (await sender.PushAsync(second)).Status);
        Assert.Equal(HttpStatusCode.NoContent, (await sender.PushAsync(first)).Status);

        var whole = await mine;
        Assert.Equal(HttpStatusCode.OK, whole.Status);
        Assert.Equal(content, whole.Body);

        var part = await theirs;
        Assert.Equal(HttpStatusCode.PartialContent, part.Status);
        Assert.Equal(content[1000..2000], part.Body);
    }

    [Fact]
    public async Task A_host_that_starts_stopping_lets_a_waiting_fetch_go()
    {
        // Its own host, because this test stops it.
        var server = await fixture.StoppableStreamsAsync();
        var party = await Party.ConnectAsync(server, "fenna");
        var (fenna, f1) = (party.Account(0), party.Client(0));

        var offered = await StreamApi.OfferAsync(
            server,
            fenna.Access,
            party.GeneralId,
            new StreamOffer { FileName = "interrupted.bin", Size = 4096 });
        Assert.Equal(HttpStatusCode.Created, offered.Status);

        var fetch = StreamApi.FetchAsync(server, fenna.Access, offered.As(StreamedFile.Parser).Id);
        await Sender.NextRequestAsync(f1);

        var watch = Stopwatch.StartNew();
        server.Services.GetRequiredService<IHostApplicationLifetime>().StopApplication();

        // Either the shutting-down answer or a connection that went with the host; never a
        // request left waiting out its sender timeout on a server that is going away.
        var finished = await Task.WhenAny(fetch, Task.Delay(Promptly));
        Assert.True(finished == fetch, $"the fetch was still waiting after {watch.Elapsed.TotalSeconds:0.#}s");
        try
        {
            var answer = await fetch;
            Assert.Equal(HttpStatusCode.ServiceUnavailable, answer.Status);
            Assert.Equal("server shutting down", answer.Detail);
        }
        catch (Exception ex) when (ex is HttpRequestException or IOException or OperationCanceledException or InvalidOperationException)
        {
            // The host took the connection down before the answer could be written.
        }
    }

    private static async Task<T> WithinAsync<T>(Task<T> task, string what)
    {
        var finished = await Task.WhenAny(task, Task.Delay(Promptly));
        Assert.True(finished == task, $"{what} was still waiting after {Promptly.TotalSeconds:0.#}s");
        return await task;
    }

    private async Task<StreamedFile> OfferAsync(Account owner, long channelId, long size)
    {
        var response = await StreamApi.OfferAsync(
            Server,
            owner.Access,
            channelId,
            new StreamOffer { FileName = "file.bin", ContentType = StreamApi.Octets, Size = size });
        Assert.True(response.Status == HttpStatusCode.Created, $"offer in {channelId}: {(int)response.Status}");
        return response.As(StreamedFile.Parser);
    }
}
