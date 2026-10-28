<!-- Author: Nicola Antonello -->

# Benchmarking

Measuring latency and throughput without fooling yourself. Part of the [reading list](README.md).

- ★ Gil Tene, [How NOT to Measure Latency](https://www.infoq.com/presentations/latency-response-time/) (QCon San Francisco, 2015). Coordinated omission, and why percentiles cannot be averaged.
- Gernot Heiser, [Systems Benchmarking Crimes](https://gernot-heiser.org/benchmarking-crimes.html). A checklist of misleading benchmarking practices to avoid.
- Brendan Gregg, [Systems Performance: Enterprise and the Cloud, 2nd edition](https://www.brendangregg.com/systems-performance-2nd-edition-book.html) (Addison-Wesley, 2020). Chapter 2 covers the USE method (utilization, saturation and errors for every resource), queueing theory with Little's law, and percentiles; chapter 12 covers benchmarking, its common failures, and active benchmarking.
- Marios Kogias, Stephen Mallon and Edouard Bugnion, [Lancet: A self-correcting Latency Measuring Tool](https://www.usenix.org/conference/atc19/presentation/kogias-lancet) (USENIX ATC, 2019). Open-loop load at a fixed rate with a confidence interval for each percentile, reporting nothing rather than a wrong p99 when samples are not independent, and showing how clients that cannot keep up turn an open-loop test into a closed-loop one that under-reports the tail.
- Aleksander Maricq et al., [Taming Performance Variability](https://www.usenix.org/conference/osdi18/presentation/maricq) (OSDI, 2018). About 900,000 measurements on 835 servers: results are often not normally distributed and vary even between identical machines, so repeat each run, report the median with a nonparametric confidence interval, and choose the number of repetitions from the data.
- Jeffrey Dean and Luiz André Barroso, [The Tail at Scale](https://doi.org/10.1145/2408776.2408794) (Communications of the ACM, 2013). Why tail latency matters at scale, and techniques that tolerate latency variability.
- Bianca Schroeder, Adam Wierman and Mor Harchol-Balter, [Open Versus Closed: A Cautionary Tale](https://www.usenix.org/conference/nsdi-06/open-versus-closed-cautionary-tale) (NSDI, 2006). Closed-loop and open-loop load generators report different response times at the same load.
- John D. C. Little, [A Proof for the Queuing Formula: L = λW](https://doi.org/10.1287/opre.9.3.383) (Operations Research, 1961). The origin of Little's law: the average number of requests in a system equals their arrival rate times the average time each spends there, which links a load test's concurrency, throughput and latency.
