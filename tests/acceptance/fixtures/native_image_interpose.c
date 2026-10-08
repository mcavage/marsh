#include <unistd.h>
#include <sys/socket.h>
#include <stdlib.h>
#include <fcntl.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/clonefile.h>
#include <sys/stat.h>
#include <crt_externs.h>
#define INTERPOSE(new,old) __attribute__((used)) static struct { const void *a,*b; } ip_##old __attribute__((section("__DATA,__interpose"))) = { (const void *)&new,(const void *)&old }
static void logline(const char *kind,const char *path) { const char *log=getenv("BRUSH_PROBE_LOG"); if(!log)return;int fd=open(log,O_WRONLY|O_CREAT|O_APPEND,0600);if(fd>=0){dprintf(fd,"%s %d %s\n",kind,getpid(),path?path:"");close(fd);} }
static int mode(const char *name){const char *s=getenv("BRUSH_PROBE_MODE");return s && !strcmp(s,name);}
static char *created(char *path){char *r=mkdtemp(path);if(r && strstr(r,"/brush-regex-image-"))logline("DIR",r);return r;}
INTERPOSE(created,mkdtemp);
static int permissions(int fd,const char *path,mode_t mask,int flags){if((mode("setup-fail")||mode("setup-cleanup-fail")) && strstr(path,"/brush-regex-image-")){errno=EMFILE;return -1;}return fchmodat(fd,path,mask,flags);}
INTERPOSE(permissions,fchmodat);
static int linking(int srcfd,const char *src,int dstfd,const char *dst,int flags){
 if(!strcmp(dst,"image")){
  if(mode("force-copy")||mode("clone-fail")){errno=EXDEV;return -1;}
  if(mode("wrong-link")){const char *wrong=getenv("BRUSH_PROBE_WRONG");if(wrong)return linkat(AT_FDCWD,wrong,dstfd,dst,0);}
 }
 int result=linkat(srcfd,src,dstfd,dst,flags); if(result==0 && !strcmp(dst,"image")){struct stat st;if(fstatat(dstfd,dst,&st,AT_SYMLINK_NOFOLLOW)==0){char msg[128];snprintf(msg,sizeof(msg),"dev=%llu ino=%llu mode=%o",(unsigned long long)st.st_dev,(unsigned long long)st.st_ino,st.st_mode & 07777);logline("DELEGATED_LINK",msg);}}return result;
}
INTERPOSE(linking,linkat);
static int cloning(int src,int dst,const char *name,int flags){logline("CLONE_ATTEMPT",name);if(mode("force-copy")){errno=ENOTSUP;return -1;}if(mode("clone-fail")||mode("create-cleanup-fail")){errno=EACCES;return -1;}return fclonefileat(src,dst,name,flags);}
INTERPOSE(cloning,fclonefileat);
static int executing(const char *path,char *const argv[],char *const envp[]){if(mode("exec-fail") && strstr(path,"/brush-regex-image-")){errno=EACCES;return -1;}return execve(path,argv,envp);}
INTERPOSE(executing,execve);
static ssize_t writing(int fd,const void *bytes,size_t length){if(mode("bad-ready") && length==8 && !memcmp(bytes,"SBXNRDY2",8))return write(fd,"BROKEN!!",8);return write(fd,bytes,length);}
INTERPOSE(writing,write);
static int deleting(int fd,const char *name,int flags){if(mode("cleanup-fail") && !strcmp(name,"image")){errno=EACCES;return -1;}return unlinkat(fd,name,flags);}
INTERPOSE(deleting,unlinkat);
static int removing(const char *path){if((mode("create-cleanup-fail")||mode("setup-cleanup-fail")) && strstr(path,"/brush-regex-image-")){errno=EACCES;return -1;}return rmdir(path);}
INTERPOSE(removing,rmdir);
static int count_directories(void) {
 const char *p=getenv("BRUSH_PROBE_LOG"); if(!p)return 0;
 FILE *f=fopen(p,"r");if(!f)return 0;char line[4096];int n=0;
 while(fgets(line,sizeof(line),f)){if(!strncmp(line,"DIR ",4))n++;}fclose(f);return n;
}
__attribute__((constructor)) static void delay(void){char **a=*_NSGetArgv();if(a[1] && !strcmp(a[1],"--brush-subshell-fd") && mode("partial-pipeline")){logline("CHILD",a[0]);if(count_directories()==2)_exit(66);}if(a[1] && !strcmp(a[1],"--brush-subshell-fd") && mode("early-exit")){logline("CHILD",a[0]);_exit(123);}if(a[1] && !strcmp(a[1],"--brush-subshell-fd") && (mode("delay")||mode("cancel"))){logline("CHILD",a[0]);sleep(7);}}

static ssize_t sending(int fd,const void *bytes,size_t length,int flags){
 if(mode("bad-ready") && length==8 && !memcmp(bytes,"SBXNRDY2",8)){logline("CORRUPTED_READY","");return send(fd,"BROKEN!!",8,flags);}
 return send(fd,bytes,length,flags);
}
INTERPOSE(sending,send);
