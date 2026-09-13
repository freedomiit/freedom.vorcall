using System.Net;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.DependencyInjection;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Streams;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

// A streamed file's bytes never touch this server: the row is the whole of what it keeps, and a
// reader's GET meets the owning client's chunk push in memory. Every test here therefore drives
// both halves at once — a fetch that is still in flight while the owner answers the StreamRequest
// the server sent down its socket.
[Collection(ServerCollection.Name)]
public sealed class StreamsTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task An_offer_names_a_text_channel_its_owner_may_attach_in_and_a_size_a_file_could_have()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "dana");
        var (owner, dana) = (party.Account(0), party.Account(1));
        var (_, channel) = await party.CreateChannelAsync();

        var offered = await StreamApi.OfferAsync(
            Server,
            dana.Access,
            channel,
            new StreamOffer { FileName = "holiday.mkv", ContentType = "VIDEO/x-Matroska", Size = 12_000_000_000 });
        Assert.Equal(HttpStatusCode.Created, offered.Status);
        var file = offered.As(StreamedFile.Parser);
        Assert.True(file.Id > 0);
        Assert.Equal("holiday.mkv", file.FileName);
        Assert.Equal("video/x-matroska", file.ContentType);
        Assert.Equal(12_000_000_000, file.Size);
        Assert.Equal(dana.UserId, file.OwnerId);

        // The file name is metadata and never a path, and an offer that declares no type is bytes
        // of an unstated kind.
        var sanitised = await StreamApi.OfferAsync(
            Server,
            dana.Access,
            channel,
            new StreamOffer { FileName = "../../etc/passwd", Size = 1 });
        Assert.Equal(HttpStatusCode.Created, sanitised.Status);
        Assert.Equal("passwd", sanitised.As(StreamedFile.Parser).FileName);
        Assert.Equal(StreamApi.Octets, sanitised.As(StreamedFile.Parser).ContentType);

        // The channel is a numeric id and has to hold messages for anything to link the offer to.
        foreach (var bad in new[] { "bad!", "0", History.Id(party.GeneralVoiceId) })
        {
            var refused = await StreamApi.OfferAsync(Server, dana.Access, bad, new StreamOffer { FileName = "a", Size = 1 });
            Assert.True(refused.Status == HttpStatusCode.BadRequest, $"'{bad}': {(int)refused.Status}");
            Assert.Equal("channel", refused.Detail);
        }

        // A size of zero or less is not a file, and one over a tebibyte is a client's mistake.
        foreach (var size in new[] { 0L, -1L })
        {
            var refused = await StreamApi.OfferAsync(Server, dana.Access, channel, new StreamOffer { FileName = "a", Size = size });
            Assert.True(refused.Status == HttpStatusCode.BadRequest, $"{size}: {(int)refused.Status}");
            Assert.Equal("size", refused.Detail);
        }

        var huge = await StreamApi.OfferAsync(
            Server,
            dana.Access,
            channel,
            new StreamOffer { FileName = "a", Size = StreamOptions.MaxFileBytes + 1 });
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, huge.Status);
        Assert.Equal("files must be 1 TiB or smaller", huge.Detail);

        // The rule an attachment's declared type follows: any media type, but a media type.
        var malformed = await StreamApi.OfferAsync(
            Server,
            dana.Access,
            channel,
            new StreamOffer { FileName = "a", ContentType = "video/x-matroska; codecs=vp9", Size = 1 });
        Assert.Equal(HttpStatusCode.BadRequest, malformed.Status);
        Assert.Equal("malformed content type", malformed.Detail);

        // Sight of the channel, then the right to attach in it: the same two refusals an upload
        // answers, each naming the bit that is missing.
        await party.SetMemberOverrideAsync(channel, 1, deny: (ulong)Perm.AttachFiles);
        var cannotAttach = await StreamApi.OfferAsync(Server, dana.Access, channel, new StreamOffer { FileName = "a", Size = 1 });
        Assert.Equal(HttpStatusCode.Forbidden, cannotAttach.Status);
        Assert.Equal(PermNames.Name(Perm.AttachFiles), cannotAttach.Detail);

        await party.Client(0).SendAsync(
            Frames.SetOverride(channel, Frames.MemberOverride(dana.UserId, deny: (ulong)Perm.ViewChannel)));
        await party.Client(1).ExpectChannelDeletedAsync(channel);
        await party.Client(0).ExpectChannelUpsertedAsync(channel);

        var hidden = await StreamApi.OfferAsync(Server, dana.Access, channel, new StreamOffer { FileName = "a", Size = 1 });
        Assert.Equal(HttpStatusCode.Forbidden, hidden.Status);
        Assert.Equal(PermNames.Name(Perm.ViewChannel), hidden.Detail);
        _ = owner;
    }

    [Fact]
    public async Task An_unlinked_offer_belongs_to_its_owner_alone_and_a_linked_one_to_the_channels_viewers()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "elsa", "farah");
        var (owner, elsa, farah) = (party.Account(0), party.Account(1), party.Account(2));
        var (o1, e1) = (party.Client(0), party.Client(1));
        var (_, channel) = await party.CreateChannelAsync();
        var content = Blob.Of(4096);
        var file = await OfferAsync(elsa, channel, content.Length, "notes.bin");
        var sender = new Sender(Server, elsa, content);

        var unknown = await StreamApi.FetchAsync(Server, farah.Access, file.Id + 1_000_000);
        Assert.Equal(HttpStatusCode.NotFound, unknown.Status);
        Assert.Equal("no such streamed file", unknown.Detail);

        // Before a message names it, an offer belongs to whoever made it — the server's owner
        // included, since this is ownership of a file and not a permission.
        foreach (var stranger in new[] { farah, owner })
        {
            var refused = await StreamApi.FetchAsync(Server, stranger.Access, file.Id);
            Assert.True(refused.Status == HttpStatusCode.Forbidden, $"{stranger.Username}: {(int)refused.Status}");
            Assert.Equal("not yours", refused.Detail);
        }

        var text = $"have this {Names.Token()}";
        await e1.SendAsync(Frames.SendStreamed(text, channel, file.Id));
        await party.ExpectMessageEverywhereAsync(text, channel);

        // After, it belongs to the channel that message is in.
        var reader = StreamApi.FetchAsync(Server, farah.Access, file.Id);
        await sender.ServeAsync(e1);
        var served = await reader;
        Assert.Equal(HttpStatusCode.OK, served.Status);
        Assert.Equal(content, served.Body);

        await o1.SendAsync(Frames.SetOverride(channel, Frames.MemberOverride(farah.UserId, deny: (ulong)Perm.ViewChannel)));
        await party.Client(2).ExpectChannelDeletedAsync(channel);
        await o1.ExpectChannelUpsertedAsync(channel);
        await e1.ExpectChannelUpsertedAsync(channel);

        var lost = await StreamApi.FetchAsync(Server, farah.Access, file.Id);
        Assert.Equal(HttpStatusCode.Forbidden, lost.Status);
        Assert.Equal(PermNames.Name(Perm.ViewChannel), lost.Detail);
    }

    [Fact]
    public async Task A_whole_fetch_carries_the_proxys_headers_and_exactly_the_bytes_the_owner_pushed()
    {
        await using var party = await Party.ConnectAsync(Server, "gilda");
        var (gilda, g1) = (party.Account(0), party.Client(0));
        var content = Blob.Of(300_000);
        var file = await OfferAsync(gilda, party.GeneralId, content.Length, "report.pdf", "application/pdf");
        var sender = new Sender(Server, gilda, content);

        var fetch = StreamApi.FetchAsync(Server, gilda.Access, file.Id);
        var request = await Sender.NextRequestAsync(g1);
        Assert.Equal(file.Id, request.StreamId);
        Assert.True(request.TransferId > 0, "the transfer has no id");
        Assert.Equal(0, request.Offset);
        Assert.Equal(content.Length, request.Length);

        var push = await sender.PushAsync(request);
        Assert.Equal(HttpStatusCode.NoContent, push.Status);

        var served = await fetch;
        Assert.Equal(HttpStatusCode.OK, served.Status);
        Assert.Equal(content, served.Body);
        Assert.Equal("application/pdf", served.Header("Content-Type"));
        Assert.Equal(content.Length.ToString(System.Globalization.CultureInfo.InvariantCulture), served.Header("Content-Length"));
        Assert.Equal("bytes", served.Header("Accept-Ranges"));

        // Live bytes off somebody else's disk: nothing to cache, nothing to sniff, nothing to
        // render inline on this origin.
        Assert.Equal(["no-store", "private"], served.Parts("Cache-Control"));
        Assert.Equal("nosniff", served.Header("X-Content-Type-Options"));
        Assert.Equal("attachment", served.Header("Content-Disposition"));
    }

    [Fact]
    public async Task A_range_is_asked_of_the_owner_and_answered_as_the_partial_content_it_promised()
    {
        await using var party = await Party.ConnectAsync(Server, "hilda");
        var (hilda, h1) = (party.Account(0), party.Client(0));
        var content = Blob.Of(1000);
        var file = await OfferAsync(hilda, party.GeneralId, content.Length);
        var sender = new Sender(Server, hilda, content);

        var (open, openRequest) = await TransferAsync(sender, h1, hilda.Access, file.Id, "bytes=100-");
        Assert.Equal(HttpStatusCode.PartialContent, open.Status);
        Assert.Equal(content[100..], open.Body);
        Assert.Equal("900", open.Header("Content-Length"));
        Assert.Equal("bytes 100-999/1000", open.Header("Content-Range"));
        Assert.Equal(100, openRequest.Offset);
        Assert.Equal(900, openRequest.Length);

        var (middle, middleRequest) = await TransferAsync(sender, h1, hilda.Access, file.Id, "bytes=10-19");
        Assert.Equal(HttpStatusCode.PartialContent, middle.Status);
        Assert.Equal(content[10..20], middle.Body);
        Assert.Equal("bytes 10-19/1000", middle.Header("Content-Range"));
        Assert.Equal(10, middleRequest.Offset);
        Assert.Equal(10, middleRequest.Length);

        // bytes=-N is the last N bytes.
        var (tail, tailRequest) = await TransferAsync(sender, h1, hilda.Access, file.Id, "bytes=-16");
        Assert.Equal(HttpStatusCode.PartialContent, tail.Status);
        Assert.Equal(content[^16..], tail.Body);
        Assert.Equal("bytes 984-999/1000", tail.Header("Content-Range"));
        Assert.Equal(984, tailRequest.Offset);
        Assert.Equal(16, tailRequest.Length);

        // An end past the last byte is clamped rather than refused, and so is a suffix longer
        // than the file.
        var (clamped, clampedRequest) = await TransferAsync(sender, h1, hilda.Access, file.Id, "bytes=990-4000");
        Assert.Equal(HttpStatusCode.PartialContent, clamped.Status);
        Assert.Equal(content[990..], clamped.Body);
        Assert.Equal(10, clampedRequest.Length);

        var (everything, everythingRequest) = await TransferAsync(sender, h1, hilda.Access, file.Id, "bytes=-5000");
        Assert.Equal(HttpStatusCode.PartialContent, everything.Status);
        Assert.Equal(content, everything.Body);
        Assert.Equal("bytes 0-999/1000", everything.Header("Content-Range"));
        Assert.Equal(0, everythingRequest.Offset);
        Assert.Equal(content.Length, everythingRequest.Length);

        // No range, several ranges, and a unit that is not bytes are all the whole file.
        foreach (var range in new[] { null, "bytes=0-1,4-5", "items=0-1", "bytes=oops" })
        {
            var (whole, wholeRequest) = await TransferAsync(sender, h1, hilda.Access, file.Id, range);
            Assert.True(whole.Status == HttpStatusCode.OK, $"'{range}': {(int)whole.Status}");
            Assert.Null(whole.Header("Content-Range"));
            Assert.Equal(content, whole.Body);
            Assert.Equal(0, wholeRequest.Offset);
            Assert.Equal(content.Length, wholeRequest.Length);
        }
    }

    [Fact]
    public async Task A_range_that_starts_past_the_end_is_unsatisfiable_and_the_owner_is_never_asked()
    {
        await using var party = await Party.ConnectAsync(Server, "ilse");
        var (ilse, i1) = (party.Account(0), party.Client(0));
        var file = await OfferAsync(ilse, party.GeneralId, 1000);

        foreach (var range in new[] { "bytes=1000-", "bytes=1000-2000", "bytes=4000-", "bytes=-0" })
        {
            var refused = await StreamApi.FetchAsync(Server, ilse.Access, file.Id, range);
            Assert.True(refused.Status == HttpStatusCode.RequestedRangeNotSatisfiable, $"'{range}': {(int)refused.Status}");
            Assert.Equal("range not satisfiable", refused.Detail);
            Assert.Equal("bytes */1000", refused.Header("Content-Range"));
        }

        // The range is resolved before anything is registered, so no transfer was ever opened and
        // the owner was never disturbed.
        await i1.QuietAsync();
    }

    [Fact]
    public async Task A_decline_ends_the_fetch_with_the_reason_the_owner_gave()
    {
        await using var party = await Party.ConnectAsync(Server, "jutta");
        var (jutta, j1) = (party.Account(0), party.Client(0));
        var file = await OfferAsync(jutta, party.GeneralId, 4096);
        var sender = new Sender(Server, jutta, Blob.Of(4096));

        var fetch = StreamApi.FetchAsync(Server, jutta.Access, file.Id);
        var request = await Sender.NextRequestAsync(j1);
        var declined = await sender.DeclineAsync(request, "the file has moved\n");
        Assert.Equal(HttpStatusCode.NoContent, declined.Status);

        var answer = await fetch;
        Assert.Equal(HttpStatusCode.Gone, answer.Status);
        Assert.Equal("the file has moved", answer.Detail);

        // The transfer is gone from the registry, so the push that follows a decline finds
        // nothing rather than racing it.
        var late = await sender.PushAsync(request);
        Assert.Equal(HttpStatusCode.NotFound, late.Status);
        Assert.Equal("no such transfer", late.Detail);

        // A decline with nothing to say still says something.
        var second = StreamApi.FetchAsync(Server, jutta.Access, file.Id);
        var again = await Sender.NextRequestAsync(j1);
        Assert.Equal(HttpStatusCode.NoContent, (await sender.DeclineAsync(again)).Status);
        Assert.Equal("the sender declined", (await second).Detail);

        // And a reason no error detail could carry is cut to length rather than refused.
        var third = StreamApi.FetchAsync(Server, jutta.Access, file.Id);
        var last = await Sender.NextRequestAsync(j1);
        Assert.Equal(HttpStatusCode.NoContent, (await sender.DeclineAsync(last, new string('r', 400))).Status);
        Assert.Equal(new string('r', 200), (await third).Detail);
    }

    [Fact]
    public async Task A_push_answers_only_its_own_transfer_and_only_once()
    {
        await using var party = await Party.ConnectAsync(Server, "kira", "lotte");
        var (kira, k1, lotte) = (party.Account(0), party.Client(0), party.Account(1));
        var content = Blob.Of(2048);
        var file = await OfferAsync(kira, party.GeneralId, content.Length);
        var other = await OfferAsync(kira, party.GeneralId, content.Length);
        var sender = new Sender(Server, kira, content);

        // A transfer id is digits, and a transfer nobody holds is nobody's.
        foreach (var raw in new[] { string.Empty, "-1", " 7", "x" })
        {
            var refused = await StreamApi.PushAsync(Server, kira.Access, file.Id, raw, content);
            Assert.True(refused.Status == HttpStatusCode.BadRequest, $"'{raw}': {(int)refused.Status}");
            Assert.Equal("transfer", refused.Detail);
        }

        var unknown = await StreamApi.PushAsync(Server, kira.Access, file.Id, "999999999", content);
        Assert.Equal(HttpStatusCode.NotFound, unknown.Status);
        Assert.Equal("no such transfer", unknown.Detail);

        var fetch = StreamApi.FetchAsync(Server, kira.Access, file.Id);
        var request = await Sender.NextRequestAsync(k1);

        // Somebody else's transfer, and the right transfer under the wrong stream, are both
        // simply unknown: an answer to a transfer that is not yours learns nothing about it.
        var foreign = await StreamApi.PushAsync(Server, lotte.Access, file.Id, StreamApi.Id(request.TransferId), content);
        Assert.Equal(HttpStatusCode.NotFound, foreign.Status);
        Assert.Equal("no such transfer", foreign.Detail);

        var wrongStream = await StreamApi.PushAsync(Server, kira.Access, other.Id, StreamApi.Id(request.TransferId), content);
        Assert.Equal(HttpStatusCode.NotFound, wrongStream.Status);

        Assert.Equal(HttpStatusCode.NoContent, (await sender.PushAsync(request)).Status);
        Assert.Equal(content, (await fetch).Body);

        // The transfer is claimed once, and it stayed in the registry for as long as the copy ran,
        // so a second push is refused rather than starting another one.
        var again = await sender.PushAsync(request);
        Assert.True(
            again.Status is HttpStatusCode.Conflict or HttpStatusCode.NotFound,
            $"a second push answered {(int)again.Status}");
    }

    [Fact]
    public async Task A_second_push_of_a_transfer_whose_copy_is_still_running_is_refused()
    {
        await using var party = await Party.ConnectAsync(Server, "vesna");
        var (vesna, v1) = (party.Account(0), party.Client(0));

        // Large enough that the copy is still running while the second push is made: a transfer
        // stays in the registry until its push ends, so that the refusal holds for that whole time.
        var content = Blob.Of(16 << 20);
        var file = await OfferAsync(vesna, party.GeneralId, content.Length);
        var sender = new Sender(Server, vesna, content);

        var open = StreamApi.OpenAsync(Server, vesna.Access, file.Id);
        var request = await Sender.NextRequestAsync(v1);
        var push = sender.PushAsync(request);

        using var response = await open;
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var body = await response.Content.ReadAsStreamAsync(CancellationToken.None);
        var head = new byte[8192];
        await body.ReadExactlyAsync(head);
        Assert.False(push.IsCompleted, "the copy finished before the second push could be made");

        var second = await sender.PushAsync(request, content);
        Assert.Equal(HttpStatusCode.Conflict, second.Status);
        Assert.Equal("transfer already answered", second.Detail);

        // The first push is untouched by the second and still finishes the range it holds.
        using var rest = new MemoryStream();
        await body.CopyToAsync(rest);
        Assert.Equal(content[head.Length..], rest.ToArray());
        Assert.Equal(HttpStatusCode.NoContent, (await push).Status);
    }

    [Fact]
    public async Task An_owner_that_is_already_offline_is_never_asked_for_a_range()
    {
        await using var party = await Party.ConnectAsync(Server, "wilja");
        var (wilja, w1) = (party.Account(0), party.Client(0));
        var file = await OfferAsync(wilja, party.GeneralId, 4096);

        await w1.CloseAsync();
        var refused = await StreamApi.FetchAsync(Server, wilja.Access, file.Id);
        Assert.Equal(HttpStatusCode.Conflict, refused.Status);
        Assert.Equal("the sender is offline", refused.Detail);

        // Nothing was queued for the account either: the next session it opens is not handed a
        // request for a reader that gave up long ago.
        await using var back = await WsClient.ConnectAsync(Server, wilja);
        await back.QuietAsync();
    }

    [Fact]
    public async Task A_push_shorter_than_the_range_aborts_the_reader_and_a_longer_one_is_cut_to_it()
    {
        await using var party = await Party.ConnectAsync(Server, "maren");
        var (maren, m1) = (party.Account(0), party.Client(0));
        var content = Blob.Of(5000);
        var file = await OfferAsync(maren, party.GeneralId, content.Length);
        var sender = new Sender(Server, maren, content);

        // The Content-Length is already promised, so a response that simply ended would read as a
        // whole file: the reader's connection is aborted instead.
        var short1 = StreamApi.FetchAsync(Server, maren.Access, file.Id);
        var request = await Sender.NextRequestAsync(m1);
        var pushed = await sender.PushAsync(request, content[..1000]);
        Assert.Equal(HttpStatusCode.BadRequest, pushed.Status);
        Assert.Equal("body ended early", pushed.Detail);
        await Assert.ThrowsAnyAsync<Exception>(() => short1);

        // A body past the range is cut to it: the reader gets what its Content-Length said and
        // nothing more.
        var long1 = StreamApi.FetchAsync(Server, maren.Access, file.Id, "bytes=0-999");
        var ranged = await Sender.NextRequestAsync(m1);
        Assert.Equal(1000, ranged.Length);
        var overrun = await sender.PushAsync(ranged, content);
        Assert.Equal(HttpStatusCode.NoContent, overrun.Status);
        var served = await long1;
        Assert.Equal(HttpStatusCode.PartialContent, served.Status);
        Assert.Equal(content[..1000], served.Body);
    }

    [Fact]
    public async Task A_message_may_name_four_streamed_files_of_its_senders_own_in_this_channel_and_no_others()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "nadia", "orla");
        var (nadia, n1, orla) = (party.Account(1), party.Client(1), party.Account(2));
        var (_, channel) = await party.CreateChannelAsync();
        var (_, elsewhere) = await party.CreateChannelAsync();

        var mine = await OfferAsync(nadia, channel, 1024);
        var second = await OfferAsync(nadia, channel, 1024);
        var otherChannel = await OfferAsync(nadia, elsewhere, 1024);
        var foreign = await OfferAsync(orla, channel, 1024);
        var before = (await History.PageAsync(Server, nadia.Access, channel)).Messages.Count;

        var five = Enumerable.Range(0, 5).Select(offset => mine.Id + offset).ToArray();
        foreach (var (name, ids) in new (string, long[])[]
        {
            ("five", five),
            ("duplicates", [mine.Id, mine.Id]),
            ("unknown", [mine.Id + 1_000_000]),
            ("another member's", [foreign.Id]),
            ("another channel's", [otherChannel.Id]),
        })
        {
            await n1.SendAsync(Frames.SendStreamed($"{name} {Names.Token()}", channel, ids));
            var error = await n1.ExpectErrorAsync(ErrorCode.InvalidStream, fatal: false);
            Assert.Equal("streamed file is unknown, not yours, not in this channel or already used", error.Detail);
        }

        // Nothing was written down and nothing was claimed: the offers are all still unlinked.
        Assert.Equal(before, (await History.PageAsync(Server, nadia.Access, channel)).Messages.Count);
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<Vorcall.Server.Data.AppDbContext>>();
        await using (var db = await contexts.CreateDbContextAsync())
        {
            var ids = new[] { mine.Id, second.Id, otherChannel.Id, foreign.Id };
            Assert.Empty(await db.StreamedFiles.AsNoTracking().Where(s => ids.Contains(s.Id) && s.MessageId != null).ToListAsync());
        }

        // Text may be empty, and only then, when the message carries files instead.
        await n1.SendAsync(Frames.SendStreamed(string.Empty, channel, mine.Id, second.Id));
        var message = (await party.ExpectEverywhereAsync(Kind.Message))[0].Message;
        Assert.Equal(string.Empty, message.Text);
        Assert.Equal(new[] { mine.Id, second.Id }, message.StreamedFiles.Select(file => file.Id).ToArray());

        // An offer a message already names may not be named again.
        await n1.SendAsync(Frames.SendStreamed("again", channel, mine.Id));
        await n1.ExpectErrorAsync(ErrorCode.InvalidStream, fatal: false);
    }

    [Fact]
    public async Task A_streamed_file_travels_with_its_message_through_the_broadcast_the_history_and_an_edit()
    {
        await using var party = await Party.ConnectAsync(Server, "petia", "quilla");
        var (petia, p1, quilla) = (party.Account(0), party.Client(0), party.Account(1));
        var general = party.GeneralId;

        // Offered in one order and named in another: the message carries the sender's order, not
        // the row order.
        var first = await OfferAsync(petia, general, 111, "first.bin", "text/plain");
        var secondFile = await OfferAsync(petia, general, 222, "second.bin");
        var text = $"two files {Names.Token()}";
        await p1.SendAsync(Frames.SendStreamed(text, general, secondFile.Id, first.Id));

        long messageId = 0;
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.Message))
        {
            var message = frame.Message;
            Assert.Equal(text, message.Text);
            Assert.Equal(new[] { secondFile.Id, first.Id }, message.StreamedFiles.Select(file => file.Id).ToArray());
            Assert.Equal("second.bin", message.StreamedFiles[0].FileName);
            Assert.Equal(222, message.StreamedFiles[0].Size);
            Assert.Equal("text/plain", message.StreamedFiles[1].ContentType);
            Assert.Equal(petia.UserId, message.StreamedFiles[1].OwnerId);
            messageId = message.Id;
        }

        var page = await History.ByIdAsync(Server, quilla.Access, general);
        Assert.Equal(
            new[] { first.Id, secondFile.Id },
            page[messageId].StreamedFiles.Select(file => file.Id).Order().ToArray());

        await p1.SendAsync(Frames.Edit(messageId, "two files, still"));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.MessageEdited))
        {
            Assert.Equal(
                new[] { first.Id, secondFile.Id },
                frame.MessageEdited.Message.StreamedFiles.Select(file => file.Id).Order().ToArray());
        }
    }

    [Fact]
    public async Task Deleting_the_message_takes_its_streamed_files_with_it_and_so_does_deleting_the_channel()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "rhea");
        var (o1, rhea, r1) = (party.Client(0), party.Account(1), party.Client(1));
        var (_, channel) = await party.CreateChannelAsync();
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<Vorcall.Server.Data.AppDbContext>>();

        var deleted = await OfferAsync(rhea, channel, 512);
        await r1.SendAsync(Frames.SendStreamed("bye", channel, deleted.Id));
        var messageId = await party.ExpectMessageEverywhereAsync("bye", channel);

        await r1.SendAsync(Frames.Delete(messageId));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.MessageDeleted))
        {
            Assert.Equal(messageId, frame.MessageDeleted.Id);
        }

        // A tombstone keeps its row and loses everything hanging off it.
        var tombstone = (await History.ByIdAsync(Server, rhea.Access, channel))[messageId];
        Assert.True(tombstone.Deleted, "the tombstone is not marked deleted");
        Assert.Empty(tombstone.StreamedFiles);

        var cascaded = await OfferAsync(rhea, channel, 512);
        await r1.SendAsync(Frames.SendStreamed("later", channel, cascaded.Id));
        await party.ExpectMessageEverywhereAsync("later", channel);

        await o1.SendAsync(Frames.DeleteChannel(channel));
        await party.ExpectEverywhereAsync(Kind.ChannelDeleted);

        await using var db = await contexts.CreateDbContextAsync();
        var ids = new[] { deleted.Id, cascaded.Id };
        Assert.Empty(await db.StreamedFiles.AsNoTracking().Where(s => ids.Contains(s.Id)).ToListAsync());

        var gone = await StreamApi.FetchAsync(Server, rhea.Access, deleted.Id);
        Assert.Equal(HttpStatusCode.NotFound, gone.Status);
    }

    [Fact]
    public async Task A_message_carrying_a_streamed_file_needs_ATTACH_FILES_like_any_other()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "sonja");
        var (sonja, s1) = (party.Account(1), party.Client(1));
        var (_, channel) = await party.CreateChannelAsync();
        var file = await OfferAsync(sonja, channel, 256);

        // The offer was made while the bit was there; losing it before the message is sent is
        // what the frame's own check is for.
        await party.SetMemberOverrideAsync(channel, 1, deny: (ulong)Perm.AttachFiles);
        await s1.SendAsync(Frames.SendStreamed("here", channel, file.Id));
        var error = await s1.ExpectErrorAsync(ErrorCode.PermissionDenied, fatal: false);
        Assert.Equal(PermNames.Name(Perm.AttachFiles), error.Detail);
    }

    [Fact]
    public async Task An_owner_that_never_answers_gives_up_the_fetch_and_a_second_reader_is_refused_over_the_cap()
    {
        // A one-second sender timeout and one transfer per owner: both are the shared host's
        // defaults turned down to something a test can reach.
        var server = await fixture.TightStreamsAsync();
        await using var party = await Party.ConnectAsync(server, "tilda");
        var (tilda, t1) = (party.Account(0), party.Client(0));

        var response = await StreamApi.OfferAsync(
            server,
            tilda.Access,
            party.GeneralId,
            new StreamOffer { FileName = "quiet.bin", Size = 4096 });
        Assert.Equal(HttpStatusCode.Created, response.Status);
        var file = response.As(StreamedFile.Parser);

        var first = StreamApi.FetchAsync(server, tilda.Access, file.Id);
        var request = await Sender.NextRequestAsync(t1);

        // The owner already serves as many transfers as it may, so the second reader is refused
        // rather than the owner being asked for one more range.
        var second = await StreamApi.FetchAsync(server, tilda.Access, file.Id);
        Assert.Equal(HttpStatusCode.ServiceUnavailable, second.Status);
        Assert.Equal("the sender is serving too many transfers", second.Detail);

        var timedOut = await first;
        Assert.Equal(HttpStatusCode.GatewayTimeout, timedOut.Status);
        Assert.Equal("the sender did not answer", timedOut.Detail);

        // The abandoned transfer freed the owner's one slot, and its id names nothing now.
        var late = await StreamApi.PushAsync(server, tilda.Access, file.Id, StreamApi.Id(request.TransferId), Blob.Of(4096));
        Assert.Equal(HttpStatusCode.NotFound, late.Status);

        var third = StreamApi.FetchAsync(server, tilda.Access, file.Id);
        var asked = await Sender.NextRequestAsync(t1);
        Assert.Equal(
            HttpStatusCode.NoContent,
            (await StreamApi.PushAsync(server, tilda.Access, file.Id, StreamApi.Id(asked.TransferId), Blob.Of(4096))).Status);
        Assert.Equal(HttpStatusCode.OK, (await third).Status);
    }

    [Fact]
    public async Task With_streamed_files_off_none_of_the_four_routes_exists()
    {
        var server = await fixture.StreamsDisabledAsync();
        await using var party = await Party.ConnectAsync(server, "ulrika");
        var ulrika = party.Account(0);

        var offer = await StreamApi.OfferAsync(server, ulrika.Access, party.GeneralId, new StreamOffer { FileName = "a", Size = 1 });
        Assert.Equal(HttpStatusCode.NotFound, offer.Status);

        var fetch = await StreamApi.FetchAsync(server, ulrika.Access, 1);
        Assert.Equal(HttpStatusCode.NotFound, fetch.Status);

        var push = await StreamApi.PushAsync(server, ulrika.Access, 1, "1", Blob.Of(8));
        Assert.Equal(HttpStatusCode.NotFound, push.Status);

        var decline = await StreamApi.DeclineAsync(server, ulrika.Access, 1, "1");
        Assert.Equal(HttpStatusCode.NotFound, decline.Status);

        // An unmapped route is the door key middleware's plain 404, not an endpoint's protobuf
        // body, which is how a client tells "off" from "refused".
        Assert.Empty(offer.Body);
    }

    private async Task<StreamedFile> OfferAsync(
        Account owner,
        long channelId,
        long size,
        string fileName = "file.bin",
        string contentType = StreamApi.Octets)
    {
        var response = await StreamApi.OfferAsync(
            Server,
            owner.Access,
            channelId,
            new StreamOffer { FileName = fileName, ContentType = contentType, Size = size });
        Assert.True(response.Status == HttpStatusCode.Created, $"offer in {channelId}: {(int)response.Status}");
        return response.As(StreamedFile.Parser);
    }

    // Both halves at once: the fetch goes out, the owner answers the range it is asked for, and
    // only then is there a response to read.
    private async Task<(ProtoResponse Answer, StreamRequest Request)> TransferAsync(
        Sender sender,
        WsClient socket,
        string bearer,
        long id,
        string? range = null)
    {
        var fetch = StreamApi.FetchAsync(Server, bearer, id, range);
        var request = await sender.ServeAsync(socket);
        return (await fetch, request);
    }
}
