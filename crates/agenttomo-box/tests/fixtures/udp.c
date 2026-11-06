#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static struct sockaddr_in peers[2];
static int fd;
static void check(long result, const char *operation) {
    if(result<0) {perror(operation);exit(91);}
}
static void source(const struct sockaddr_in *actual, unsigned which) {
    if(actual->sin_family!=AF_INET || actual->sin_port!=peers[which].sin_port || actual->sin_addr.s_addr!=peers[which].sin_addr.s_addr) {
        fprintf(stderr,"wrong reply source: port %u, expected %u\n",ntohs(actual->sin_port),ntohs(peers[which].sin_port));exit(92);
    }
}
static void send_one(const void *data, size_t length, unsigned which) {
    if(sendto(fd,data,length,0,(void*)&peers[which],sizeof(peers[which]))!=(ssize_t)length) {perror("sendto");exit(93);}
}
static void receive_one(const void *expected, size_t length, unsigned which) {
    char data[1024];struct sockaddr_in actual;socklen_t size=sizeof(actual);
    ssize_t count=recvfrom(fd,data,sizeof(data),0,(void*)&actual,&size);check(count,"recvfrom");
    if(count!=(ssize_t)length || memcmp(data,expected,length) || size!=sizeof(actual)) exit(94);
    source(&actual,which);
}
static void connected(void) {
    check(connect(fd,(void*)&peers[0],sizeof(peers[0])),"connect");
    struct sockaddr_in actual;socklen_t size=sizeof(actual);
    check(getpeername(fd,(void*)&actual,&size),"getpeername");source(&actual,0);
}
static void message(void) {
    char a[]="hello",b[]="udp";struct iovec iov[2]={{a,5},{b,3}};
    void *page=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    if(page==MAP_FAILED)exit(95);
    struct msghdr *msg=page;struct sockaddr_in *destination=(void*)((char*)page+128);
    *destination=peers[0];*msg=(struct msghdr){.msg_name=destination,.msg_namelen=sizeof(*destination),.msg_iov=iov,.msg_iovlen=2};
    check(mprotect(page,4096,PROT_READ),"readonly header");
    if(sendmsg(fd,msg,0)!=8)exit(96);
    if(msg->msg_name!=destination || memcmp(destination,&peers[0],sizeof(*destination)))exit(97);
    char bytes[32];struct iovec input={bytes,sizeof(bytes)};struct sockaddr_in actual;
    struct msghdr reply={.msg_name=&actual,.msg_namelen=sizeof(actual),.msg_iov=&input,.msg_iovlen=1};
    if(recvmsg(fd,&reply,0)!=8 || memcmp(bytes,"helloudp",8))exit(98);
    source(&actual,0);munmap(page,4096);
}
static void batch(int partial) {
    char *data[3]={"first","","third"};struct iovec iov[3];struct mmsghdr msg[3]={0};
    for(int i=0;i<3;i++) {iov[i]=(struct iovec){data[i],strlen(data[i])};msg[i].msg_hdr=(struct msghdr){.msg_name=&peers[i%2],.msg_namelen=sizeof(peers[0]),.msg_iov=&iov[i],.msg_iovlen=1};}
    if(partial)msg[1].msg_hdr.msg_name=(void*)1;
    int count=sendmmsg(fd,msg,3,0);check(count,"sendmmsg");if(count!=(partial?1:3))exit(99);
    for(int i=0;i<count;i++)if(msg[i].msg_len!=strlen(data[i]) || msg[i].msg_hdr.msg_name!=&peers[i%2])exit(100);
    if(partial) {receive_one("first",5,0);return;}
    int received=0;unsigned seen=0;
    while(received<3) {
        char buffers[3][32];struct sockaddr_in actual[3];struct iovec input[3];struct mmsghdr replies[3]={0};
        for(int i=0;i<3;i++){input[i]=(struct iovec){buffers[i],32};replies[i].msg_hdr=(struct msghdr){.msg_name=&actual[i],.msg_namelen=sizeof(actual[i]),.msg_iov=&input[i],.msg_iovlen=1};}
        int n=recvmmsg(fd,replies,3-received,MSG_WAITFORONE,NULL);check(n,"recvmmsg");
        for(int i=0;i<n;i++){unsigned which=replies[i].msg_len==0?1:0;source(&actual[i],which);if(replies[i].msg_len==0)seen|=2;else if(!memcmp(buffers[i],"first",5))seen|=1;else if(!memcmp(buffers[i],"third",5))seen|=4;else exit(101);}
        received+=n;
    }
    if(seen!=7)exit(102);
}
static void *sender(void *argument) {
    unsigned which=(unsigned)(long)argument;
    send_one(which?"one":"zero",which?3:4,which);
    return NULL;
}
static int ipv6(const char *port) {
    struct sockaddr_in6 peer={.sin6_family=AF_INET6,.sin6_port=htons(atoi(port)),.sin6_addr=IN6ADDR_LOOPBACK_INIT};
    struct sockaddr_in6 local={.sin6_family=AF_INET6,.sin6_addr=IN6ADDR_LOOPBACK_INIT},actual;
    int sock=socket(AF_INET6,SOCK_DGRAM,0);check(sock,"socket6");
    check(bind(sock,(void*)&local,sizeof(local)),"bind6");socklen_t size=sizeof(local);
    check(getsockname(sock,(void*)&local,&size),"getsockname6");printf("PORT=%u\n",ntohs(local.sin6_port));fflush(stdout);
    struct timeval timeout={.tv_sec=3};check(setsockopt(sock,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout)),"timeout6");
    check(connect(sock,(void*)&peer,sizeof(peer)),"connect6");size=sizeof(actual);
    check(getpeername(sock,(void*)&actual,&size),"getpeername6");if(memcmp(&actual,&peer,sizeof(peer)))return 110;
    if(send(sock,"ipv6",4,0)!=4)return 111;
    char bytes[8];size=sizeof(actual);
    if(recvfrom(sock,bytes,8,0,(void*)&actual,&size)!=4 || memcmp(bytes,"ipv6",4) || memcmp(&actual,&peer,sizeof(peer)))return 112;
    close(sock);puts("UDP_OK");return 0;
}
int main(int argc,char **argv) {
    if(argc!=4)return 90;
    if(!strcmp(argv[1],"ipv6"))return ipv6(argv[2]);
    for(int i=0;i<2;i++){peers[i].sin_family=AF_INET;peers[i].sin_port=htons(atoi(argv[i+2]));inet_pton(AF_INET,"127.0.0.1",&peers[i].sin_addr);}
    fd=socket(AF_INET,SOCK_DGRAM,0);check(fd,"socket");
    struct sockaddr_in local={.sin_family=AF_INET,.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
    check(bind(fd,(void*)&local,sizeof(local)),"bind");socklen_t size=sizeof(local);check(getsockname(fd,(void*)&local,&size),"getsockname");
    printf("PORT=%u\n",ntohs(local.sin_port));fflush(stdout);
    struct timeval timeout={.tv_sec=3};check(setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout)),"timeout");
    const char *mode=argv[1];
    if(!strcmp(mode,"message"))message();
    else if(!strcmp(mode,"batch") || !strcmp(mode,"partial"))batch(!strcmp(mode,"partial"));
    else if(!strcmp(mode,"shared")){
        pthread_t threads[2];for(long i=0;i<2;i++)if(pthread_create(&threads[i],0,sender,(void*)i))exit(103);
        for(int i=0;i<2;i++)pthread_join(threads[i],0);
        for(int i=0;i<2;i++){char bytes[8];struct sockaddr_in actual;size=sizeof(actual);ssize_t n=recvfrom(fd,bytes,8,0,(void*)&actual,&size);check(n,"shared receive");source(&actual,n==3?1:0);}
    }
    else if(!strcmp(mode,"trunc")) {
        char bytes[64];memset(bytes,'a',sizeof(bytes));send_one(bytes,sizeof(bytes),0);
        char small[8];struct sockaddr_in actual;size=sizeof(actual);
        if(recvfrom(fd,small,8,MSG_PEEK,(void*)&actual,&size)!=8){exit(104);}source(&actual,0);
        struct iovec iov={small,8};struct msghdr reply={.msg_name=&actual,.msg_namelen=sizeof(actual),.msg_iov=&iov,.msg_iovlen=1};
        if(recvmsg(fd,&reply,MSG_TRUNC)!=64 || !(reply.msg_flags&MSG_TRUNC)){exit(105);}source(&actual,0);
        send_one("",0,0);receive_one("",0,0);
    }
    else if(!strcmp(mode,"errors")) {
        char data[8];errno=0;if(recvfrom(fd,data,8,MSG_DONTWAIT,NULL,NULL)!=-1 || errno!=EAGAIN)exit(106);
        if(sendto(fd,"x",1,0,(void*)1,sizeof(peers[0]))!=-1 || errno!=EFAULT)exit(107);
        send_one("valid",5,0);receive_one("valid",5,0);
    }
    else if(!strcmp(mode,"connected") || !strcmp(mode,"background")) {
        connected();
        if(!strcmp(mode,"background")){pid_t child=fork();check(child,"fork");if(child){close(fd);return 23;}int alias=dup(fd);check(alias,"dup");close(fd);fd=alias;usleep(200000);}
        if(write(fd,"write",5)!=5){exit(108);}receive_one("write",5,0);
        struct iovec iov[2]={{"vec",3},{"tor",3}};if(writev(fd,iov,2)!=6)exit(109);receive_one("vector",6,0);
        struct sockaddr unspecified={.sa_family=AF_UNSPEC};check(connect(fd,&unspecified,sizeof(unspecified)),"disconnect");
        send_one("new",3,1);receive_one("new",3,1);
    }
    else if(!strcmp(mode,"dns")) {
        const unsigned char query[]={0x12,0x34,1,0,0,1,0,0,0,0,0,0,4,'t','e','s','t',0,0,1,0,1};
        const unsigned char answer[]={0xc0,0x0c,0,1,0,1,0,0,0,60,0,4,127,0,0,42};
        unsigned char response[sizeof(query)+sizeof(answer)];memcpy(response,query,sizeof(query));
        response[2]=0x81;response[3]=0x80;response[7]=1;memcpy(response+sizeof(query),answer,sizeof(answer));
        send_one(query,sizeof(query),0);receive_one(response,sizeof(response),0);
    }
    else if(!strcmp(mode,"short_source")) {
        send_one("first",5,0);send_one("second",6,1);
        for(int i=0;i<2;i++){
            unsigned char actual[16];memset(actual,0xa5,sizeof(actual));size=2;char data[16];
            check(recvfrom(fd,data,sizeof(data),0,(void*)actual,&size),"short recvfrom");
            if(size!=sizeof(peers[0]) || *(sa_family_t*)actual!=AF_INET)exit(113);
            for(unsigned j=2;j<sizeof(actual);j++)if(actual[j]!=0xa5)exit(114);
        }
    }
    else if(!strcmp(mode,"partial_receive")) {
        void *page=mmap(NULL,8192,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);if(page==MAP_FAILED)exit(115);
        struct mmsghdr *reply=(void*)((char*)page+4096-sizeof(*reply));
        char data[32];struct iovec iov={data,sizeof(data)};struct sockaddr_in actual;
        reply->msg_hdr=(struct msghdr){.msg_name=&actual,.msg_namelen=sizeof(actual),.msg_iov=&iov,.msg_iovlen=1};
        check(munmap((char*)page+4096,4096),"unmap second header");
        send_one("first",5,0);struct pollfd pollfd={fd,POLLIN,0};if(poll(&pollfd,1,3000)!=1)exit(116);
        if(recvmmsg(fd,reply,2,MSG_DONTWAIT,NULL)!=1 || reply->msg_len!=5 || memcmp(data,"first",5))exit(117);
        source(&actual,0);munmap(page,4096);
    }
    else {send_one("first",5,0);send_one("second",6,1);for(int i=0;i<2;i++){char data[8];struct sockaddr_in actual;size=sizeof(actual);ssize_t n=recvfrom(fd,data,8,0,(void*)&actual,&size);check(n,"receive");source(&actual,n==5?0:1);}}
    close(fd);puts("UDP_OK");return 0;
}
